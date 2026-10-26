use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use showmesh_ndi::config::{Config, DisplayConfig, OutputConfig, SinkKind, SourceConfig};
use showmesh_ndi::display::Display;
use showmesh_ndi::{api, discovery, init_gstreamer, manager::Manager};

#[derive(Parser)]
#[command(version, about = "Plays NDI sources fullscreen on this machine's display outputs")]
struct Cli {
    #[arg(long, short, default_value = "/etc/showmesh-ndi/config.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the receiver service and REST API (default).
    Serve,
    /// List physical display outputs and their modes.
    Outputs {
        #[arg(long)]
        card: Option<PathBuf>,
    },
    /// List NDI sources visible on the network.
    Sources {
        /// Seconds to listen before printing.
        #[arg(long, default_value_t = 5)]
        wait: u64,
    },
    /// Play one source on one output without the API or saved configuration.
    Play {
        #[arg(long)]
        output: String,
        #[arg(long, conflicts_with = "pattern")]
        ndi_name: Option<String>,
        #[arg(long, conflicts_with = "pattern")]
        address: Option<String>,
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long)]
        mode: Option<String>,
        #[arg(long, value_enum, default_value = "kms")]
        sink: Sink,
        #[arg(long)]
        card: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Sink {
    Kms,
    Wayland,
    Fake,
}

impl From<Sink> for SinkKind {
    fn from(s: Sink) -> Self {
        match s {
            Sink::Kms => SinkKind::Kms,
            Sink::Wayland => SinkKind::Wayland,
            Sink::Fake => SinkKind::Fake,
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
    let cli = Cli::parse();
    init_gstreamer()?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(cli.config),
        Command::Outputs { card } => {
            let display = Display::open(&DisplayConfig { sink: SinkKind::Wayland, card, ..Default::default() })?;
            let outputs = display.outputs()?;
            if outputs.is_empty() {
                println!("No display outputs found.");
            }
            for o in outputs {
                let state = if o.connected { "connected" } else { "disconnected" };
                println!("{} ({state})", o.name);
                for m in o.modes {
                    println!("    {}{}", m.name, if m.preferred { " preferred" } else { "" });
                }
            }
            Ok(())
        }
        Command::Sources { wait } => {
            let d = discovery::Discovery::start();
            std::thread::sleep(Duration::from_secs(wait));
            let status = d.status();
            if let Some(m) = status.message {
                println!("{m}");
            }
            if status.running && status.sources.is_empty() {
                println!("No NDI sources found.");
            }
            for s in status.sources {
                println!("{}{}", s.name, s.address.map(|a| format!("  ({a})")).unwrap_or_default());
            }
            Ok(())
        }
        Command::Play { output, ndi_name, address, pattern, mode, sink, card } => {
            let source = match pattern {
                Some(pattern) => SourceConfig::TestPattern { pattern },
                None => SourceConfig::Ndi { name: ndi_name, address },
            };
            let config = Config {
                display: DisplayConfig { sink: sink.into(), card, ..Default::default() },
                outputs: vec![OutputConfig { connector: output, enabled: true, mode, source: Some(source) }],
                ..Default::default()
            };
            config.validate()?;
            let manager = Manager::new(config, None)?;
            manager.start();
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(shutdown_signal());
            manager.shutdown();
            Ok(())
        }
    }
}

fn serve(path: PathBuf) -> Result<()> {
    let config = Config::load(&path)?;
    tracing::info!(config = %path.display(), version = env!("CARGO_PKG_VERSION"), "starting showmesh-ndi");
    let listen = config.api.listen.clone();
    let manager = Manager::new(config, Some(path))?;
    manager.start();
    let rt = tokio::runtime::Runtime::new()?;
    let m: Arc<Manager> = manager.clone();
    rt.block_on(async move {
        let listener =
            tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("listening on {listen}"))?;
        tracing::info!("REST API listening on http://{listen}");
        axum::serve(listener, api::router(m)).with_graceful_shutdown(shutdown_signal()).await?;
        anyhow::Ok(())
    })?;
    manager.shutdown();
    Ok(())
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
