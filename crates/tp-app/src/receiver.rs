use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::sync::CancellationToken;
use tp_media::{AudioOutput, AudioReceiver, AudioReceiverConfig, VideoReceiver, VideoReceiverConfig};
use tp_proto::{ControlMessage, Endpoint, EndpointConfig, MediaEvent, Session, StreamKind, generate_pairing_code};

use crate::events::{Event, StatsSnapshot};
use crate::sink::{AudioBlock, FrameSink, VideoFrame};
use crate::store::{JsonPeerStore, load_identity, role_dir};

/// Where decoded audio is played, besides the frame sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioOut {
    /// VB-Cable's "CABLE Input" when installed, otherwise nowhere.
    Auto,
    /// The system default output.
    Default,
    None,
    /// First output whose name contains this text.
    Named(String),
}

impl std::str::FromStr for AudioOut {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" => AudioOut::Auto,
            "default" => AudioOut::Default,
            "none" => AudioOut::None,
            _ => AudioOut::Named(s.to_string()),
        })
    }
}

#[derive(Clone)]
pub struct ReceiverConfig {
    pub port: u16,
    pub name: String,
    pub audio_out: AudioOut,
    pub config_dir: PathBuf,
    pub sink: Arc<dyn FrameSink>,
    /// Publish over mDNS. Off in tests so parallel runs do not see each other.
    pub advertise: bool,
    /// Also gets every decoded video frame, at full size and rate, for a
    /// front end that shows the stream. The CLI has nowhere to show it.
    pub preview: Option<Arc<dyn FrameSink>>,
    pub cancel: CancellationToken,
}

/// A pairing code stays valid this long if nobody uses it.
const PAIRING_CODE_LIFETIME: Duration = Duration::from_secs(300);
/// Minimum gap between codes opened by failed connection attempts. Each code
/// survives only three wrong guesses; without this a LAN attacker could keep
/// asking for fresh codes and walk the code space.
const PAIRING_REOPEN_GAP: Duration = Duration::from_secs(10);
/// The receiver asks for at most one keyframe per this interval, so a burst
/// of loss does not turn into a burst of IDRs that causes more loss.
const KEYFRAME_REQUEST_GAP: Duration = Duration::from_millis(250);
const LEVEL_INTERVAL: Duration = Duration::from_millis(50);

struct Pairing {
    expires: Option<Instant>,
    last_opened: Option<Instant>,
}

impl Pairing {
    fn open(&mut self, ep: &Endpoint, tx: &UnboundedSender<Event>) {
        let code = generate_pairing_code();
        ep.set_pairing_code(Some(code.clone()));
        let now = Instant::now();
        self.expires = Some(now + PAIRING_CODE_LIFETIME);
        self.last_opened = Some(now);
        tracing::info!("pairing code: {code}");
        let _ = tx.send(Event::PairingCode(Some(code)));
    }

    /// Reflects a code the endpoint consumed or abandoned on its own.
    fn sync(&mut self, ep: &Endpoint, tx: &UnboundedSender<Event>) {
        if self.expires.is_some() && !ep.pairing_open() {
            self.expires = None;
            let _ = tx.send(Event::PairingCode(None));
        }
    }

    fn may_reopen(&self) -> bool {
        self.last_opened.is_none_or(|t| t.elapsed() >= PAIRING_REOPEN_GAP)
    }
}

pub async fn run_receiver(cfg: ReceiverConfig, tx: UnboundedSender<Event>) -> anyhow::Result<()> {
    let dir = role_dir(&cfg.config_dir, "receiver");
    let identity = load_identity(&dir)?;
    let store = Arc::new(JsonPeerStore::open(dir.join("peers.json"))?);
    let fingerprint = identity.fingerprint();
    let ep = Endpoint::receiver(
        EndpointConfig::new(identity, cfg.name.clone(), store.clone()),
        SocketAddr::from(([0, 0, 0, 0], cfg.port)),
    )
    .with_context(|| format!("binding UDP port {}", cfg.port))?;
    let port = ep.local_addr()?.port();
    tracing::info!("receiver listening on UDP {port}, fingerprint {fingerprint}");
    let _ = tx.send(Event::Listening { port, fingerprint: fingerprint.to_hex() });

    let _advertiser = if cfg.advertise {
        let ad = tp_discovery::Advertisement {
            name: cfg.name.clone(),
            port,
            install_id: fingerprint.to_hex()[..16].to_string(),
            fingerprint: fingerprint.to_hex(),
            paired: !store.is_empty(),
        };
        match tp_discovery::Advertiser::start(&ad) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!("mDNS advertisement failed, senders must connect by address: {e}");
                None
            }
        }
    } else {
        None
    };

    // Device enumeration takes a noticeable moment; do it once, not per session.
    let audio_out = cfg.audio_out.clone();
    let audio_output = tokio::task::spawn_blocking(move || resolve_audio_out(&audio_out)).await?;

    let mut pairing = Pairing { expires: None, last_opened: None };
    if store.is_empty() {
        pairing.open(&ep, &tx);
    }
    let _ = tx.send(Event::Status("Waiting for a sender".into()));

    loop {
        let expiry = pairing.expires;
        let expire = async {
            match expiry {
                Some(t) => tokio::time::sleep_until(t.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = cfg.cancel.cancelled() => break,
            _ = expire => {
                ep.set_pairing_code(None);
                pairing.sync(&ep, &tx);
            }
            res = ep.accept() => {
                pairing.sync(&ep, &tx);
                match res {
                    Ok(session) => {
                        let reason = handle_session(session, &cfg, audio_output.clone(), &tx).await;
                        tracing::info!("session ended: {reason}");
                        let _ = tx.send(Event::Disconnected { reason });
                        let _ = tx.send(Event::Status("Waiting for a sender".into()));
                    }
                    Err(tp_proto::Error::Closed) => break,
                    Err(e) => {
                        // A failed handshake is usually an unpaired sender, so
                        // this is the moment to show a code.
                        tracing::info!("connection attempt failed: {e}");
                        if !ep.pairing_open() && pairing.may_reopen() {
                            pairing.open(&ep, &tx);
                        }
                    }
                }
            }
        }
    }
    ep.close();
    let _ = tokio::time::timeout(Duration::from_secs(1), ep.wait_idle()).await;
    Ok(())
}

fn resolve_audio_out(out: &AudioOut) -> Option<AudioOutput> {
    let found = |r: tp_media::Result<Option<tp_media::DeviceInfo>>, what: &str| match r {
        Ok(Some(d)) => {
            tracing::info!("decoded audio plays into \"{}\"", d.name);
            Some(AudioOutput::Device(d))
        }
        Ok(None) => {
            tracing::warn!("{what} not found; decoded audio goes only to the frame sink");
            None
        }
        Err(e) => {
            tracing::warn!("listing audio outputs failed: {e}");
            None
        }
    };
    match out {
        AudioOut::Auto => found(tp_media::find_vb_cable_input(), "VB-Cable (CABLE Input)"),
        AudioOut::Named(n) => found(tp_media::find_audio_output(n), n),
        AudioOut::Default => Some(AudioOutput::Default),
        AudioOut::None => None,
    }
}

#[derive(Default)]
struct Counters {
    decoded_frames: AtomicU64,
    received_bytes: AtomicU64,
}

/// Runs one connected sender until it leaves, the link dies, or we are
/// cancelled. Returns a human-readable reason.
async fn handle_session(
    session: Session,
    cfg: &ReceiverConfig,
    audio_output: Option<AudioOutput>,
    tx: &UnboundedSender<Event>,
) -> String {
    let session = Arc::new(session);
    let peer = session.peer_name().to_string();
    let addr = session.remote_address();
    tracing::info!("connected: {peer} at {addr} ({})", session.peer_fingerprint());
    let _ = tx.send(Event::Connected { peer: peer.clone(), addr: addr.to_string() });
    let _ = tx.send(Event::Status(format!("Receiving from {peer}")));

    // recv_control is not cancel safe (a dropped read loses part of a frame),
    // so it gets its own task and the main loop selects on a channel instead.
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
    let ctrl_session = session.clone();
    let ctrl_task = tokio::spawn(async move {
        while let Ok(Some(m)) = ctrl_session.recv_control().await {
            if ctrl_tx.send(m).is_err() {
                break;
            }
        }
    });

    // The sender opens with StreamConfig; its audio channel count shapes the
    // Opus decoder caps.
    let mut channels = 1;
    let mut fps = 0u32;
    // The sender announces only after its caps negotiate, which follows
    // device enumeration and pipeline start, so allow it a few seconds.
    match tokio::time::timeout(Duration::from_secs(6), ctrl_rx.recv()).await {
        Ok(Some(ControlMessage::StreamConfig { video, audio })) => {
            tracing::info!("stream config: video {video:?}, audio {audio:?}");
            if let Some(v) = &video {
                fps = v.fps;
                let _ = tx.send(Event::VideoFormat { width: v.width, height: v.height, fps: v.fps });
            }
            if let Some(a) = audio {
                channels = a.channels.clamp(1, 2) as u32;
            }
        }
        Ok(Some(other)) => tracing::warn!("expected StreamConfig, got {other:?}"),
        Ok(None) => {
            ctrl_task.abort();
            return "peer closed the control stream".into();
        }
        Err(_) => tracing::warn!("no StreamConfig within 6s; assuming mono audio, unknown frame rate"),
    }

    let counters = Arc::new(Counters::default());
    let (vrx, arx) = match build_pipelines(cfg, channels, fps, audio_output, counters.clone(), tx) {
        Ok(p) => p,
        Err(e) => {
            ctrl_task.abort();
            session.close().await;
            let msg = format!("media pipeline failed: {e}");
            let _ = tx.send(Event::Error(msg.clone()));
            return msg;
        }
    };

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let mut last_keyframe_request: Option<Instant> = None;
    let mut prev = session.stats().await;
    let mut prev_frames = 0u64;
    let mut prev_bytes = 0u64;
    let mut prev_at = Instant::now();
    let mut first_video = true;
    let mut first_audio = true;

    let reason = loop {
        tokio::select! {
            _ = cfg.cancel.cancelled() => {
                session.close().await;
                break "stopped".to_string();
            }
            ev = session.recv_media() => match ev {
                Ok(MediaEvent::Video(f)) => {
                    counters.received_bytes.fetch_add(f.data.len() as u64, Ordering::Relaxed);
                    if first_video {
                        first_video = false;
                        tracing::info!("first video frame received ({} bytes, keyframe {})", f.data.len(), f.keyframe);
                    }
                    if let Err(e) = vrx.push(&f.data, f.timestamp_us, f.keyframe) {
                        tracing::debug!("video push: {e}");
                    }
                }
                Ok(MediaEvent::Audio(f)) => {
                    counters.received_bytes.fetch_add(f.data.len() as u64, Ordering::Relaxed);
                    if first_audio {
                        first_audio = false;
                        tracing::info!("first audio packet received");
                    }
                    if let Err(e) = arx.push(&f.data, f.timestamp_us) {
                        tracing::debug!("audio push: {e}");
                    }
                }
                Ok(MediaEvent::Loss { kind: StreamKind::Video, frames }) => {
                    tracing::debug!("lost {frames} video frame(s)");
                    if last_keyframe_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_GAP) {
                        last_keyframe_request = Some(Instant::now());
                        if let Err(e) = session.send_control(&ControlMessage::RequestKeyframe).await {
                            tracing::debug!("keyframe request: {e}");
                        }
                    }
                }
                // Opus concealment covers a lost audio packet; nothing to ask for.
                Ok(MediaEvent::Loss { kind: StreamKind::Audio, .. }) => {}
                Err(e) => break format!("connection lost: {e}"),
            },
            m = ctrl_rx.recv() => match m {
                Some(ControlMessage::Bye) | None => break "sender disconnected".to_string(),
                Some(other) => tracing::debug!("control: {other:?}"),
            },
            _ = tick.tick() => {
                if let Err(e) = vrx.check_error() {
                    tracing::warn!("video pipeline: {e}");
                }
                if let Err(e) = arx.check_error() {
                    tracing::warn!("audio pipeline: {e}");
                }
                let now = session.stats().await;
                let missing = (now.video.datagrams_missing + now.audio.datagrams_missing)
                    .saturating_sub(prev.video.datagrams_missing + prev.audio.datagrams_missing);
                let received = (now.video.datagrams_received + now.audio.datagrams_received)
                    .saturating_sub(prev.video.datagrams_received + prev.audio.datagrams_received);
                let loss = if missing + received == 0 { 0.0 } else { missing as f32 / (missing + received) as f32 };
                let rtt_ms = now.rtt.as_millis() as u32;
                prev = now;
                let secs = prev_at.elapsed().as_secs_f32().max(0.001);
                prev_at = Instant::now();
                let frames = counters.decoded_frames.load(Ordering::Relaxed);
                let bytes = counters.received_bytes.load(Ordering::Relaxed);
                let snap = StatsSnapshot {
                    fps: (frames - prev_frames) as f32 / secs,
                    kbps: (bytes - prev_bytes) as f32 * 8.0 / 1000.0 / secs,
                    loss,
                    rtt_ms,
                    target_kbps: 0,
                };
                prev_frames = frames;
                prev_bytes = bytes;
                let _ = tx.send(Event::Stats(snap));
                if let Err(e) = session.send_control(&ControlMessage::Stats { loss, rtt_ms }).await {
                    tracing::debug!("stats report: {e}");
                }
            }
        }
    };
    ctrl_task.abort();
    let _ = vrx.stop();
    let _ = arx.stop();
    reason
}

fn build_pipelines(
    cfg: &ReceiverConfig,
    channels: u32,
    fps: u32,
    audio_output: Option<AudioOutput>,
    counters: Arc<Counters>,
    tx: &UnboundedSender<Event>,
) -> tp_media::Result<(VideoReceiver, AudioReceiver)> {
    let sink = cfg.sink.clone();
    let preview = cfg.preview.clone();
    let first_decoded = std::sync::Once::new();
    let vrx = VideoReceiver::new(
        VideoReceiverConfig::default(),
        Box::new(move |d| {
            counters.decoded_frames.fetch_add(1, Ordering::Relaxed);
            first_decoded.call_once(|| tracing::info!("first frame decoded: {}x{}", d.width, d.height));
            let frame = VideoFrame {
                width: d.width,
                height: d.height,
                stride: d.stride,
                uv_offset: d.uv_offset,
                data: &d.data,
                pts_us: d.pts_us,
                fps_num: fps,
                fps_den: if fps > 0 { 1 } else { 0 },
            };
            sink.video(&frame);
            if let Some(p) = &preview {
                p.video(&frame);
            }
        }),
    )?;
    tracing::info!("video decoder: {}", vrx.decoder().factory_name());

    let sink = cfg.sink.clone();
    let atx = tx.clone();
    let level = Mutex::new((Instant::now(), 0f32));
    let first_decoded = std::sync::Once::new();
    let arx = AudioReceiver::new(
        AudioReceiverConfig { channels, output: audio_output },
        Box::new(move |a| {
            first_decoded.call_once(|| {
                tracing::info!("first audio block decoded: {} Hz, {} ch", a.rate, a.channels)
            });
            sink.audio(&AudioBlock { rate: a.rate, channels: a.channels, samples: &a.samples, pts_us: a.pts_us });
            let peak = a.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
            let mut l = level.lock().unwrap();
            l.1 = l.1.max(peak);
            if l.0.elapsed() >= LEVEL_INTERVAL {
                let _ = atx.send(Event::AudioLevel(l.1.min(1.0)));
                *l = (Instant::now(), 0.0);
            }
        }),
    )?;
    vrx.start()?;
    arx.start()?;
    Ok((vrx, arx))
}
