use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use serde::Serialize;

use crate::config::{ModeSpec, NdiConfig, OutputConfig, SourceConfig};
use crate::display::{Display, PrepareError, Target};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No source is assigned.
    Idle,
    /// The receiver was stopped through the API or configuration.
    Stopped,
    /// The display is unplugged or off.
    WaitingForOutput,
    /// The pipeline is running and waiting for the first frame.
    Connecting,
    Playing,
    /// The last attempt ended; a new one starts after a short delay.
    Reconnecting,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReceiverStatus {
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceConfig>,
    /// Display mode actually set on the output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<String>,
    /// Format of the video reaching the display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoFormat>,
    /// Frames per second reaching the display, measured over the last second.
    pub fps: f64,
    pub frames_received: u64,
    pub frames_displayed: u64,
    pub frames_dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_frame_age_ms: Option<u64>,
    /// Pipeline attempts after the first, since this receiver was started.
    pub restarts: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VideoFormat {
    pub width: i32,
    pub height: i32,
    pub framerate: String,
    pub format: String,
}

impl ReceiverStatus {
    fn new(source: Option<SourceConfig>) -> Self {
        Self {
            state: State::Connecting,
            message: None,
            source,
            output_mode: None,
            video: None,
            fps: 0.0,
            frames_received: 0,
            frames_displayed: 0,
            frames_dropped: 0,
            last_frame_age_ms: None,
            restarts: 0,
        }
    }
}

/// One output's playback thread. Each output has its own pipeline, so one failure stays on that output.
pub struct Receiver {
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<ReceiverStatus>>,
    thread: Option<JoinHandle<()>>,
}

impl Receiver {
    pub fn spawn(output: OutputConfig, display: Arc<Display>, ndi: NdiConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(ReceiverStatus::new(output.source.clone())));
        let ctx = Ctx { output, display, ndi, stop: stop.clone(), status: status.clone() };
        let thread = thread::Builder::new()
            .name(format!("rx-{}", ctx.output.connector))
            .spawn(move || ctx.run())
            .expect("spawning receiver thread");
        Self { stop, status, thread: Some(thread) }
    }

    pub fn status(&self) -> ReceiverStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

enum Outcome {
    Stopped,
    SourceEnded { played: bool },
    OutputLost,
    Failed { played: bool, message: String },
}

struct Ctx {
    output: OutputConfig,
    display: Arc<Display>,
    ndi: NdiConfig,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<ReceiverStatus>>,
}

const MAX_BACKOFF: Duration = Duration::from_secs(5);

impl Ctx {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn set(&self, state: State, message: Option<String>) {
        let mut s = self.status.lock().unwrap();
        if s.state != state || s.message != message {
            match &message {
                Some(m) => tracing::info!(output = %self.output.connector, ?state, "{m}"),
                None => tracing::info!(output = %self.output.connector, ?state, "state changed"),
            }
        }
        s.state = state;
        s.message = message;
        if state != State::Playing {
            s.fps = 0.0;
        }
    }

    /// Sleeps up to `d`, returning early if stopped. Returns true when stopped.
    fn sleep(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.stopped() {
                return true;
            }
            thread::sleep(Duration::from_millis(100).min(end - Instant::now()));
        }
        self.stopped()
    }

    fn run(self) {
        let mode = self.output.mode.as_deref().and_then(|m| ModeSpec::parse(m).ok());
        let mut backoff = Duration::from_millis(500);
        let mut attempts = 0u64;
        while !self.stopped() {
            let target = match self.display.prepare(&self.output.connector, mode) {
                Ok(t) => t,
                Err(PrepareError::Disconnected) => {
                    self.set(
                        State::WaitingForOutput,
                        Some(format!("Display {} is not connected. Connect it or turn it on.", self.output.connector)),
                    );
                    self.sleep(Duration::from_secs(2));
                    continue;
                }
                Err(PrepareError::Other(e)) => {
                    self.set(State::Error, Some(format!("{e:#}")));
                    self.sleep(MAX_BACKOFF);
                    continue;
                }
            };
            self.status.lock().unwrap().output_mode = Some(target.mode.name.clone());

            let source = match (&self.output.source, self.output.enabled) {
                (_, false) => {
                    self.set(State::Stopped, Some("Receiver is stopped. Start it to play its source.".into()));
                    self.idle();
                    continue;
                }
                (None, _) => {
                    self.set(State::Idle, Some("No source is assigned. Assign an NDI source to play.".into()));
                    self.idle();
                    continue;
                }
                (Some(s), true) => s.clone(),
            };

            if attempts > 0 {
                self.status.lock().unwrap().restarts += 1;
            }
            attempts += 1;
            self.set(State::Connecting, Some(waiting_message(&source)));
            let (played, message) = match self.play(&source, &target) {
                Outcome::Stopped => break,
                Outcome::OutputLost => continue,
                Outcome::SourceEnded { played } => (played, lost_message(&source)),
                Outcome::Failed { played, message } => (played, message),
            };
            backoff = if played { Duration::from_millis(500) } else { (backoff * 2).min(MAX_BACKOFF) };
            self.set(State::Reconnecting, Some(format!("{message} Retrying in {} s.", backoff.as_secs_f32())));
            self.sleep(backoff);
        }
    }

    /// Holds the black output until stopped or the display is unplugged.
    fn idle(&self) {
        while !self.sleep(Duration::from_secs(2)) {
            if !self.display.is_connected(&self.output.connector) {
                return;
            }
        }
    }

    fn play(&self, source: &SourceConfig, target: &Target) -> Outcome {
        let counters = Arc::new(Counters::default());
        let (pipeline, sink) = match build_pipeline(source, target, &self.display, &self.ndi, &counters) {
            Ok(p) => p,
            Err(e) => return Outcome::Failed { played: false, message: format!("{e:#}.") },
        };
        let outcome = self.watch(&pipeline, &sink, &counters);
        let _ = pipeline.set_state(gst::State::Null);
        outcome
    }

    fn watch(&self, pipeline: &gst::Pipeline, sink: &gst::Element, counters: &Counters) -> Outcome {
        if pipeline.set_state(gst::State::Playing).is_err() {
            let message = pipeline
                .bus()
                .and_then(|b| b.pop_filtered(&[gst::MessageType::Error]))
                .map(|m| error_text(&m))
                .unwrap_or_else(|| "The video pipeline could not start.".into());
            return Outcome::Failed { played: false, message };
        }
        let bus = pipeline.bus().expect("pipeline has a bus");
        let started = Instant::now();
        let mut last_tick = Instant::now();
        let mut last_displayed = 0u64;
        let mut last_frame_at: Option<Instant> = None;
        let mut last_connector_check = Instant::now();
        loop {
            if self.stopped() {
                return Outcome::Stopped;
            }
            let played = counters.displayed.load(Ordering::Relaxed) > 0;
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                match msg.view() {
                    gst::MessageView::Eos(_) => return Outcome::SourceEnded { played },
                    gst::MessageView::Error(_) => return Outcome::Failed { played, message: error_text(&msg) },
                    _ => {}
                }
            }
            let now = Instant::now();
            if now - last_tick >= Duration::from_secs(1) {
                let displayed = counters.displayed.load(Ordering::Relaxed);
                if displayed != last_displayed {
                    last_frame_at = Some(now);
                }
                let fps = (displayed - last_displayed) as f64 / (now - last_tick).as_secs_f64();
                last_displayed = displayed;
                last_tick = now;
                self.update_stats(sink, counters, fps, last_frame_at.or(Some(started)).map(|t| now - t), played);
            }
            if now - last_connector_check >= Duration::from_secs(2) {
                last_connector_check = now;
                if !self.display.is_connected(&self.output.connector) {
                    return Outcome::OutputLost;
                }
            }
        }
    }

    fn update_stats(&self, sink: &gst::Element, c: &Counters, fps: f64, age: Option<Duration>, played: bool) {
        let (rendered, sink_dropped) = sink_stats(sink);
        let received = c.received.load(Ordering::Relaxed);
        let left_queue = c.left_queue.load(Ordering::Relaxed);
        let queued = sink
            .parent()
            .and_downcast::<gst::Bin>()
            .and_then(|b| b.by_name("frames"))
            .map(|q| q.property::<u32>("current-level-buffers") as u64)
            .unwrap_or(0);
        let video = sink.static_pad("sink").and_then(|p| p.current_caps()).and_then(|caps| video_format(&caps));
        let mut s = self.status.lock().unwrap();
        s.frames_received = received;
        s.frames_displayed = rendered.unwrap_or_else(|| c.displayed.load(Ordering::Relaxed));
        // Frames the leaky queue threw away, plus frames the sink reported late.
        s.frames_dropped = received.saturating_sub(left_queue).saturating_sub(queued) + sink_dropped.unwrap_or(0);
        s.last_frame_age_ms = played.then(|| age.map(|a| a.as_millis() as u64)).flatten();
        s.video = video;
        drop(s);
        if played {
            self.set(State::Playing, None);
            self.status.lock().unwrap().fps = (fps * 100.0).round() / 100.0;
        }
    }
}

#[derive(Default)]
struct Counters {
    received: AtomicU64,
    left_queue: AtomicU64,
    displayed: AtomicU64,
}

const QUEUE_DEPTH: u64 = 2;

fn source_label(source: &SourceConfig) -> String {
    match source {
        SourceConfig::Ndi { name: Some(n), .. } => format!("NDI source \"{n}\""),
        SourceConfig::Ndi { address: Some(a), .. } => format!("NDI source at {a}"),
        SourceConfig::Ndi { .. } => "NDI source".into(),
        SourceConfig::TestPattern { pattern } => format!("Test pattern {pattern}"),
    }
}

fn waiting_message(source: &SourceConfig) -> String {
    match source {
        SourceConfig::Ndi { .. } => format!(
            "Waiting for {}. Check that the sender is running and reachable on this network.",
            source_label(source)
        ),
        SourceConfig::TestPattern { .. } => format!("Starting {}.", source_label(source)),
    }
}

fn lost_message(source: &SourceConfig) -> String {
    format!("{} stopped sending video.", source_label(source))
}

fn error_text(msg: &gst::Message) -> String {
    let gst::MessageView::Error(err) = msg.view() else {
        return "The video pipeline failed.".into();
    };
    let text = err.error().to_string();
    let details = err.debug().map(|d| d.to_string()).unwrap_or_default();
    tracing::warn!(error = %text, details = %details, "pipeline error");
    if text.contains("NDI") && (text.contains("load") || details.contains("load")) {
        return "The NDI runtime is not installed. Install it from ndi.video and restart the service.".into();
    }
    let text = text.trim_end_matches('.');
    format!("Playback failed: {text}.")
}

fn sink_stats(sink: &gst::Element) -> (Option<u64>, Option<u64>) {
    if sink.find_property("stats").is_none() {
        return (None, None);
    }
    let stats = sink.property::<gst::Structure>("stats");
    (stats.get::<u64>("rendered").ok(), stats.get::<u64>("dropped").ok())
}

fn video_format(caps: &gst::CapsRef) -> Option<VideoFormat> {
    let s = caps.structure(0)?;
    let fr = s.get::<gst::Fraction>("framerate").ok();
    Some(VideoFormat {
        width: s.get("width").ok()?,
        height: s.get("height").ok()?,
        framerate: fr.map(|f| format!("{}/{}", f.numer(), f.denom())).unwrap_or_default(),
        format: s.get::<String>("format").unwrap_or_default(),
    })
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory).name(name).build().with_context(|| {
        format!("GStreamer element {factory} is missing. Install the GStreamer plugins listed in the README")
    })
}

fn count_buffers(pad: &gst::Pad, counter: &Arc<Counters>, pick: fn(&Counters) -> &AtomicU64) {
    let counter = counter.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        pick(&counter).fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
}

/// Builds `source ! queue(leaky, 2 frames) ! videoconvert ! sink`.
fn build_pipeline(
    source: &SourceConfig,
    target: &Target,
    display: &Display,
    ndi: &NdiConfig,
    counters: &Arc<Counters>,
) -> Result<(gst::Pipeline, gst::Element)> {
    let pipeline = gst::Pipeline::with_name("receiver");
    let queue = make("queue", "frames")?;
    queue.set_property("max-size-buffers", QUEUE_DEPTH as u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property_from_str("leaky", "downstream");
    let convert = make("videoconvert", "convert")?;
    convert.set_property("n-threads", 4u32);
    let sink = build_sink(display, target)?;
    pipeline.add_many([&queue, &convert, &sink])?;
    gst::Element::link_many([&queue, &convert, &sink])?;

    count_buffers(&queue.static_pad("sink").unwrap(), counters, |c| &c.received);
    count_buffers(&queue.static_pad("src").unwrap(), counters, |c| &c.left_queue);
    count_buffers(&sink.static_pad("sink").unwrap(), counters, |c| &c.displayed);

    match source {
        SourceConfig::TestPattern { pattern } => {
            let src = make("videotestsrc", "src")?;
            src.set_property("is-live", true);
            src.set_property_from_str("pattern", pattern);
            let caps = gst::Caps::builder("video/x-raw")
                .field("width", target.mode.width as i32)
                .field("height", target.mode.height as i32)
                .field("framerate", gst::Fraction::new(target.mode.refresh as i32, 1))
                .build();
            let filter = make("capsfilter", "srccaps")?;
            filter.set_property("caps", caps);
            pipeline.add_many([&src, &filter])?;
            gst::Element::link_many([&src, &filter, &queue])?;
        }
        SourceConfig::Ndi { name, address } => {
            let src = make("ndisrc", "src")?;
            if let Some(n) = name.as_deref().filter(|n| !n.trim().is_empty()) {
                src.set_property("ndi-name", n);
            }
            if let Some(a) = address.as_deref().filter(|a| !a.trim().is_empty()) {
                src.set_property("url-address", a);
            }
            // Wait for the sender indefinitely; after it has sent, a gap of timeout_ms ends the attempt.
            src.set_property("connect-timeout", 0u32);
            src.set_property("timeout", ndi.timeout_ms);
            src.set_property("bandwidth", 100i32);
            // BGRX is what KMS planes scan out natively, so the NDI library converts once and nothing else does.
            src.set_property_from_str("color-format", "bgrx-bgra");
            let demux = make("ndisrcdemux", "demux")?;
            pipeline.add_many([&src, &demux])?;
            src.link(&demux)?;
            let queue_weak = queue.downgrade();
            let pipeline_weak = pipeline.downgrade();
            demux.connect_pad_added(move |_, pad| {
                let target = if pad.name().starts_with("video") {
                    queue_weak.upgrade()
                } else {
                    // Audio gets a sink only when it exists; an idle sink would hold back end-of-stream.
                    pipeline_weak.upgrade().and_then(|p| add_audio_sink(&p).ok())
                };
                if let Some(sinkpad) = target.and_then(|t| t.static_pad("sink"))
                    && !sinkpad.is_linked()
                    && let Err(e) = pad.link(&sinkpad)
                {
                    tracing::warn!("linking NDI {} pad failed: {e:?}", pad.name());
                }
            });
        }
    }
    Ok((pipeline, sink))
}

fn add_audio_sink(pipeline: &gst::Pipeline) -> Result<gst::Element> {
    let sink = make("fakesink", "audio")?;
    sink.set_property("sync", false);
    sink.set_property("async", false);
    pipeline.add(&sink)?;
    sink.sync_state_with_parent()?;
    Ok(sink)
}

fn build_sink(display: &Display, target: &Target) -> Result<gst::Element> {
    let sink = match display {
        Display::Kms(_, opts) => {
            let kms = target.kms.ok_or_else(|| anyhow!("output has no KMS target"))?;
            let sink = make("kmssink", "sink")?;
            sink.set_property("fd", kms.fd);
            sink.set_property("connector-id", kms.connector_id as i32);
            sink.set_property("force-modesetting", false);
            sink.set_property("restore-crtc", false);
            sink.set_property("skip-vsync", !opts.wait_vsync);
            sink
        }
        Display::Wayland(_) => {
            let sink = make("waylandsink", "sink")?;
            sink.set_property("fullscreen", true);
            sink
        }
        Display::Fake(_) => make("fakesink", "sink")?,
    };
    // Show each frame as soon as it arrives; the display's vsync paces output.
    sink.set_property("sync", false);
    sink.set_property("qos", false);
    sink.set_property("enable-last-sample", false);
    Ok(sink)
}
