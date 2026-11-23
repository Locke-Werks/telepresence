//! `telepresence`: the TelePresence sender/receiver. With no subcommand it
//! opens the Qt Quick window (when built with the `gui` feature).


use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tp_app::{AudioOut, Event, ShmSink, ReceiverConfig, SenderConfig};

#[derive(Parser)]
#[command(name = "telepresence", version, about = "Camera and microphone over the LAN")]
struct Cli {
    /// Directory for identity and pinned peers (default: platform config dir).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Accept a sender and feed its media to the virtual devices.
    Receive {
        #[arg(long, default_value_t = tp_app::DEFAULT_PORT)]
        port: u16,
        /// Name shown to senders.
        #[arg(long)]
        name: Option<String>,
        /// Where to play received audio: auto (VB-Cable if present), default, none, or a device name.
        #[arg(long, default_value = "auto")]
        audio_out: AudioOut,
        /// Do not advertise over mDNS.
        #[arg(long)]
        no_advertise: bool,
    },
    /// Capture camera and microphone and stream them to a receiver.
    Send {
        /// Receiver as ip:port, ip, or advertised name. Default: first one found.
        #[arg(long)]
        peer: Option<String>,
        /// Pairing code shown on the receiver (first connection only).
        #[arg(long)]
        code: Option<String>,
        /// Camera id or name (see `devices`). Saved for later runs.
        #[arg(long)]
        camera: Option<String>,
        /// Microphone id or name (see `devices`). Saved for later runs.
        /// "default" is refused: the system default follows AirPods.
        #[arg(long)]
        mic: Option<String>,
        /// Use test pattern and tone instead of real devices.
        #[arg(long)]
        test_source: bool,
        /// Capture size WxH, or "native" for the camera's own mode. Saved.
        #[arg(long)]
        size: Option<String>,
        /// Frame rate ceiling, or "native". Saved.
        #[arg(long)]
        fps: Option<String>,
        /// Video bitrate in kbit/s (also the adaptive ceiling), or "auto". Saved.
        #[arg(long, alias = "max-kbps")]
        bitrate: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// List cameras, microphones and audio outputs.
    Devices,
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging();
    let config_dir = cli.config_dir.unwrap_or_else(tp_app::store::config_dir);
    let Some(cmd) = cli.cmd else {
        #[cfg(feature = "gui")]
        {
            tp_app::gui::run(config_dir);
            return Ok(());
        }
        #[cfg(not(feature = "gui"))]
        {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!("\nThis build has no GUI; use a subcommand.");
            return Ok(());
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run_cli(cmd, config_dir))
}

async fn run_cli(cmd: Cmd, config_dir: PathBuf) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = match cmd {
        Cmd::Devices => return list_devices(&config_dir),
        Cmd::Receive { port, name, audio_out, no_advertise } => {
            let cfg = ReceiverConfig {
                port,
                name: name.unwrap_or_else(tp_app::default_name),
                audio_out,
                config_dir,
                sink: Arc::new(ShmSink::new()),
                advertise: !no_advertise,
                preview: None,
                cancel: cancel.clone(),
            };
            tokio::spawn(tp_app::run_receiver(cfg, tx))
        }
        Cmd::Send { peer, code, camera, mic, test_source, size, fps, bitrate, name } => {
            save_stream_flags(&config_dir, size.as_deref(), fps.as_deref(), bitrate.as_deref())?;
            let cfg = SenderConfig {
                peer,
                code,
                camera,
                mic,
                test_source,
                // Read back from the settings just written.
                size: None,
                fps: None,
                bitrate_kbps: None,
                name: name.unwrap_or_else(tp_app::default_name),
                config_dir,
                cancel: cancel.clone(),
            };
            tokio::spawn(tp_app::run_sender(cfg, tx))
        }
    };
    let ctrl_c = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("stopping");
        ctrl_c.cancel();
    });
    // The core already logs the detail; the CLI only surfaces what a person
    // at the terminal needs to act on.
    while let Some(ev) = rx.recv().await {
        match ev {
            Event::PairingCode(Some(code)) => println!("PAIRING CODE: {code}"),
            Event::NeedsPairingCode => println!("Receiver needs pairing: rerun with --code <code shown on receiver>"),
            Event::Stats(s) => tracing::info!(
                "fps {:.1}  {:.0} kbit/s  loss {:.2}%  rtt {} ms{}",
                s.fps,
                s.kbps,
                s.loss * 100.0,
                s.rtt_ms,
                if s.target_kbps > 0 { format!("  target {} kbit/s", s.target_kbps) } else { String::new() }
            ),
            Event::Error(e) => tracing::error!("{e}"),
            _ => {}
        }
    }
    task.await?
}

/// Explicit stream flags are saved like the GUI's pickers, so the next bare
/// `send` (or the GUI) keeps them. "native"/"auto" clear the saved value.
fn save_stream_flags(
    config_dir: &std::path::Path,
    size: Option<&str>,
    fps: Option<&str>,
    bitrate: Option<&str>,
) -> anyhow::Result<()> {
    if size.is_none() && fps.is_none() && bitrate.is_none() {
        return Ok(());
    }
    let dir = tp_app::store::role_dir(config_dir, "sender");
    let mut s = tp_app::store::SenderSettings::load(&dir)?;
    let auto = |v: &str| matches!(v.trim().to_ascii_lowercase().as_str(), "native" | "auto" | "");
    if let Some(v) = size {
        s.size = if auto(v) {
            None
        } else {
            let (w, h) = v
                .split_once(['x', 'X'])
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
                .ok_or_else(|| anyhow::anyhow!("--size wants WxH (e.g. 1920x1080) or native, got {v}"))?;
            Some((w, h))
        };
    }
    if let Some(v) = fps {
        s.fps = if auto(v) { None } else { Some(v.trim().parse().map_err(|_| anyhow::anyhow!("--fps wants a number or native, got {v}"))?) };
    }
    if let Some(v) = bitrate {
        s.bitrate_kbps =
            if auto(v) { None } else { Some(v.trim().parse().map_err(|_| anyhow::anyhow!("--bitrate wants kbit/s or auto, got {v}"))?) };
    }
    s.save(&dir)
}

fn list_devices(config_dir: &std::path::Path) -> anyhow::Result<()> {
    let devices = tp_media::list_devices()?;
    for kind in [tp_media::DeviceKind::Camera, tp_media::DeviceKind::Microphone, tp_media::DeviceKind::AudioOutput] {
        println!("{kind:?}:");
        for d in devices.iter().filter(|d| d.kind == kind) {
            println!("  {}  [{}]  id: {}", d.name, d.api.as_deref().unwrap_or("?"), d.id.as_deref().unwrap_or("(none)"));
        }
    }
    // What a bare `send` would capture from, without saving anything.
    let dir = tp_app::store::role_dir(config_dir, "sender");
    println!();
    match tp_app::devices::resolve(&dir, None, None, false) {
        Ok(c) => {
            println!("Sender would use:");
            println!("  camera      {} [{}] ({:?})", c.camera.name, c.camera.key, c.camera.origin);
            println!("  microphone  {} [{}] ({:?})", c.mic.name, c.mic.key, c.mic.origin);
            println!("Change with: telepresence send --camera <id|name> --mic <id|name>");
        }
        Err(e) => println!("Sender device choice: {e:#}"),
    }
    Ok(())
}
