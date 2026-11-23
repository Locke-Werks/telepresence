use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::sync::CancellationToken;
use tp_media::{AudioSource, EncodedAudio, EncodedVideo, VideoSource};
use tp_proto::{
    AudioCodec, AudioConfig, ControlMessage, Endpoint, EndpointConfig, Session, VideoCodec, VideoConfig,
};

use crate::abr::BitrateController;
use crate::events::{Event, PeerInfo, StatsSnapshot};
use crate::store::{JsonPeerStore, SenderSettings, load_identity, role_dir};

#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// `ip:port`, a bare IP (default port), or a receiver name to find over
    /// mDNS. `None` takes the first receiver discovered.
    pub peer: Option<String>,
    /// Pairing code shown on the receiver. Only needed the first time.
    pub code: Option<String>,
    /// Camera id or name. `None` uses the saved camera, or picks and saves one.
    pub camera: Option<String>,
    /// Microphone id or name, same rules as `camera`. "default" is refused.
    pub mic: Option<String>,
    /// videotestsrc and audiotestsrc instead of real devices.
    pub test_source: bool,
    /// Force a capture size. `None` takes the camera's native mode.
    pub size: Option<(u32, u32)>,
    /// Frame rate ceiling. `None` takes the camera's native rate (up to 60).
    pub fps: Option<u32>,
    /// Manual video bitrate, kbit/s: the encoder starts there and the
    /// adaptive controller never goes above it. `None` derives the start from
    /// the capture mode and lets the controller climb to 1.5x that.
    pub bitrate_kbps: Option<u32>,
    pub name: String,
    pub config_dir: PathBuf,
    pub cancel: CancellationToken,
}

const MIN_KBPS: u32 = 500;
const BACKOFF_MAX: Duration = Duration::from_secs(10);
const AUDIO_CHANNELS: u16 = 1;
/// How long to wait after start for caps to negotiate before announcing the
/// stream. Normally a few tens of milliseconds.
const FORMAT_WAIT: Duration = Duration::from_secs(3);

/// Device keys handed to the capture pipeline, resolved once per run.
#[derive(Debug, Clone)]
struct Devices {
    camera: String,
    mic: String,
}

pub async fn run_sender(cfg: SenderConfig, tx: UnboundedSender<Event>) -> anyhow::Result<()> {
    let dir = role_dir(&cfg.config_dir, "sender");
    let identity = load_identity(&dir)?;
    let store = Arc::new(JsonPeerStore::open(dir.join("peers.json"))?);
    // Flags win; anything not given comes from the saved settings, which is
    // how the GUI and a bare `telepresence send` share one configuration.
    let saved = SenderSettings::load(&dir)?;
    let mut cfg = cfg;
    cfg.size = cfg.size.or(saved.size);
    cfg.fps = cfg.fps.or(saved.fps);
    cfg.bitrate_kbps = cfg.bitrate_kbps.or(saved.bitrate_kbps);
    let cfg = cfg;
    let devices = if cfg.test_source {
        None
    } else {
        let (d, cam, mic) = (dir.clone(), cfg.camera.clone(), cfg.mic.clone());
        let choice =
            tokio::task::spawn_blocking(move || crate::devices::resolve(&d, cam.as_deref(), mic.as_deref(), true))
                .await??;
        crate::devices::log_choice(&choice);
        Some(Devices { camera: choice.camera.key, mic: choice.mic.key })
    };
    let ep = Endpoint::sender(
        EndpointConfig::new(identity, cfg.name.clone(), store),
        SocketAddr::from(([0, 0, 0, 0], 0)),
    )?;
    let mut code = cfg.code.clone();
    let mut backoff = Duration::from_secs(1);
    // Seeded from the first pipeline's capture mode and kept across
    // reconnects, so a link that was lossy a moment ago does not come back at
    // full rate.
    let mut abr: Option<BitrateController> = None;

    while !cfg.cancel.is_cancelled() {
        let addr = match resolve_peer(cfg.peer.as_deref(), &tx, &cfg.cancel).await {
            Some(a) => a,
            None if cfg.cancel.is_cancelled() => break,
            None => {
                let _ = tx.send(Event::Status("No receiver found; retrying".into()));
                if sleep_or_cancel(backoff, &cfg.cancel).await {
                    break;
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        let _ = tx.send(Event::Status(format!("Connecting to {addr}")));
        tracing::info!("connecting to {addr}{}", if code.is_some() { " with pairing code" } else { "" });
        let res = tokio::select! {
            _ = cfg.cancel.cancelled() => break,
            r = ep.connect(addr, code.as_deref()) => r,
        };
        match res {
            Ok(session) => {
                // A code is single use; reconnects rely on the pin it created.
                code = None;
                backoff = Duration::from_secs(1);
                remember_peer(&dir, addr);
                let reason = stream(session, &cfg, devices.as_ref(), &mut abr, &tx).await;
                tracing::info!("session ended: {reason}");
                let _ = tx.send(Event::Disconnected { reason });
                if cfg.cancel.is_cancelled() {
                    break;
                }
            }
            Err(e) if needs_pairing(&e) => {
                // No retry: a code has to come from the person at the receiver.
                // A pin the receiver no longer honours was already dropped.
                tracing::info!("receiver at {addr} needs a pairing code: {e}");
                let _ = tx.send(Event::NeedsPairingCode);
                anyhow::bail!("receiver at {addr} is not paired with this sender; enter the code it shows");
            }
            Err(e @ tp_proto::Error::PairingFailed(_)) => {
                // Retrying would spend another of the receiver's three guesses.
                let _ = tx.send(Event::NeedsPairingCode);
                // SPAKE2 cannot say which side was wrong without helping an
                // attacker test codes, so name the cause that is likely on a LAN.
                anyhow::bail!(
                    "pairing with {addr} failed: {e}. Most likely the code is stale: the receiver issues a \
                     new one every time it restarts. Use the code it is showing now."
                );
            }
            Err(e) => {
                tracing::warn!("connect to {addr} failed: {e}");
                let _ = tx.send(Event::Status(format!("Connect failed: {e}")));
            }
        }
        if sleep_or_cancel(backoff, &cfg.cancel).await {
            break;
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
    ep.close();
    let _ = tokio::time::timeout(Duration::from_secs(1), ep.wait_idle()).await;
    Ok(())
}

/// Lets the GUI reconnect to the same receiver on its next launch.
fn remember_peer(dir: &std::path::Path, addr: SocketAddr) {
    let res = SenderSettings::load(dir).and_then(|mut s| {
        let a = addr.to_string();
        if s.last_peer.as_deref() == Some(a.as_str()) {
            return Ok(());
        }
        s.last_peer = Some(a);
        s.save(dir)
    });
    if let Err(e) = res {
        tracing::warn!("could not record last receiver: {e:#}");
    }
}

fn needs_pairing(e: &tp_proto::Error) -> bool {
    match e {
        tp_proto::Error::NotPaired => true,
        tp_proto::Error::Rejected(r) => r.contains("not paired") || r.contains("pairing"),
        _ => false,
    }
}

/// True when cancelled.
async fn sleep_or_cancel(d: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(d) => false,
    }
}

async fn resolve_peer(peer: Option<&str>, tx: &UnboundedSender<Event>, cancel: &CancellationToken) -> Option<SocketAddr> {
    if let Some(p) = peer {
        if let Ok(a) = p.parse::<SocketAddr>() {
            return Some(a);
        }
        if let Ok(ip) = p.parse::<IpAddr>() {
            return Some(SocketAddr::new(ip, crate::DEFAULT_PORT));
        }
    }
    let wanted = peer.map(str::to_lowercase);
    let _ = tx.send(Event::Status(match &wanted {
        Some(n) => format!("Looking for {n}"),
        None => "Looking for receivers".into(),
    }));
    let found = browse_once(Duration::from_secs(5), tx.clone(), cancel.clone(), move |p| match &wanted {
        Some(n) => p.name.to_lowercase() == *n || p.id == *n,
        None => true,
    })
    .await?;
    found.addr.parse().ok()
}

/// Browses until `pick` accepts a peer or `timeout` passes, reporting every
/// peer seen on the way.
async fn browse_once(
    timeout: Duration,
    tx: UnboundedSender<Event>,
    cancel: CancellationToken,
    pick: impl Fn(&PeerInfo) -> bool + Send + 'static,
) -> Option<PeerInfo> {
    tokio::task::spawn_blocking(move || {
        let browser = match tp_discovery::Browser::start() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("mDNS browse failed: {e}");
                return None;
            }
        };
        let deadline = Instant::now() + timeout;
        while !cancel.is_cancelled() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let Some(ev) = browser.recv_timeout(left.min(Duration::from_millis(250))) else { continue };
            if let tp_discovery::DiscoveryEvent::Found(p) | tp_discovery::DiscoveryEvent::Updated(p) = ev
                && let Some(info) = peer_info(&p)
            {
                let _ = tx.send(Event::PeerFound(info.clone()));
                if pick(&info) {
                    return Some(info);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

fn peer_info(p: &tp_discovery::Peer) -> Option<PeerInfo> {
    let ip = p.addrs.first()?;
    Some(PeerInfo { id: p.id.clone(), name: p.name.clone(), addr: SocketAddr::new(*ip, p.port).to_string(), fingerprint: p.fingerprint.clone(), paired: p.paired })
}

/// Keeps reporting receivers on the LAN until cancelled, for a UI peer list.
pub fn spawn_browser(tx: UnboundedSender<Event>, cancel: CancellationToken) {
    std::thread::Builder::new()
        .name("tp-browse".into())
        .spawn(move || {
            let browser = match tp_discovery::Browser::start() {
                Ok(b) => b,
                Err(e) => {
                    let _ = tx.send(Event::Error(format!("mDNS browse failed: {e}")));
                    return;
                }
            };
            while !cancel.is_cancelled() {
                match browser.recv_timeout(Duration::from_millis(250)) {
                    Some(tp_discovery::DiscoveryEvent::Found(p) | tp_discovery::DiscoveryEvent::Updated(p)) => {
                        if let Some(info) = peer_info(&p) {
                            let _ = tx.send(Event::PeerFound(info));
                        }
                    }
                    Some(tp_discovery::DiscoveryEvent::Lost(id)) => {
                        let _ = tx.send(Event::PeerLost(id));
                    }
                    None => {}
                }
            }
        })
        .expect("spawn browse thread");
}

#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    bytes: AtomicU64,
}

async fn stream(
    session: Session,
    cfg: &SenderConfig,
    devices: Option<&Devices>,
    abr_slot: &mut Option<BitrateController>,
    tx: &UnboundedSender<Event>,
) -> String {
    let session = Arc::new(session);
    let peer = session.peer_name().to_string();
    let addr = session.remote_address();
    tracing::info!("connected to {peer} at {addr}");
    let _ = tx.send(Event::Connected { peer: peer.clone(), addr: addr.to_string() });
    let _ = tx.send(Event::Status(format!("Sending to {peer}")));

    let counters = Arc::new(Counters::default());
    let (video, audio) = match devices {
        Some(d) => (VideoSource::Named(d.camera.clone()), AudioSource::Named(d.mic.clone())),
        None => (VideoSource::TestPattern, AudioSource::TestTone),
    };
    let media_cfg = tp_media::SenderConfig {
        video: Some(video),
        audio: Some(audio),
        video_size: cfg.size,
        video_fps: cfg.fps,
        // 0 sizes the starting rate from the capture mode.
        video_bitrate_kbps: cfg.bitrate_kbps.unwrap_or(0),
        audio_channels: AUDIO_CHANNELS as u32,
        ..Default::default()
    };
    // Media is held back until StreamConfig is out, so the receiver has built
    // its pipeline before the first keyframe arrives.
    let live = Arc::new(AtomicBool::new(false));
    let vs = session.clone();
    let vc = counters.clone();
    let vlive = live.clone();
    let as_ = session.clone();
    let ac = counters.clone();
    let alive = live.clone();
    // Device lookup inside Sender::new enumerates hardware, which blocks.
    let built = tokio::task::spawn_blocking(move || {
        tp_media::Sender::new(
            media_cfg,
            Box::new(move |v: EncodedVideo| {
                if !vlive.load(Ordering::Acquire) {
                    return;
                }
                vc.frames.fetch_add(1, Ordering::Relaxed);
                vc.bytes.fetch_add(v.data.len() as u64, Ordering::Relaxed);
                if let Err(e) = vs.send_video(&v.data, v.pts_us, v.keyframe) {
                    tracing::debug!("send video: {e}");
                }
            }),
            Box::new(move |a: EncodedAudio| {
                if !alive.load(Ordering::Acquire) {
                    return;
                }
                ac.bytes.fetch_add(a.data.len() as u64, Ordering::Relaxed);
                if let Err(e) = as_.send_audio(&a.data, a.pts_us) {
                    tracing::debug!("send audio: {e}");
                }
            }),
        )
    })
    .await;
    let media = match built {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return fail(&session, tx, format!("media pipeline failed: {e}")).await,
        Err(e) => return fail(&session, tx, format!("media pipeline task failed: {e}")).await,
    };
    if let Some(enc) = media.encoder() {
        tracing::info!("video encoder: {}", enc.factory_name());
    }
    attach_level_meter(media.pipeline(), tx.clone());
    let initial = media.initial_video_bitrate_kbps().max(MIN_KBPS);
    let abr = abr_slot.get_or_insert_with(|| {
        let max = cfg.bitrate_kbps.unwrap_or(initial.saturating_mul(3) / 2).max(MIN_KBPS);
        tracing::info!("video bitrate starts at {initial} kbit/s, adaptive ceiling {max} kbit/s");
        BitrateController::new(initial, MIN_KBPS, max)
    });
    if abr.current() != media.initial_video_bitrate_kbps() {
        media.set_video_bitrate_kbps(abr.current());
    }
    if let Err(e) = media.start() {
        return fail(&session, tx, format!("starting capture failed: {e}")).await;
    }

    let deadline = Instant::now() + FORMAT_WAIT;
    let format = loop {
        if let Some(f) = media.video_format() {
            break Some(f);
        }
        // check_error consumes the bus error, so it has to be reported here.
        if let Err(e) = media.check_error() {
            let _ = media.stop();
            return fail(&session, tx, format!("capture failed: {e}")).await;
        }
        if Instant::now() >= deadline {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let video = match format {
        Some(f) => {
            let fps = if f.fps_den > 0 { (f.fps_num as f64 / f.fps_den as f64).round() as u32 } else { 0 };
            tracing::info!("stream config from video format: {}x{} at {fps} fps", f.width, f.height);
            let _ = tx.send(Event::VideoFormat { width: f.width, height: f.height, fps });
            VideoConfig { width: f.width, height: f.height, fps, codec: VideoCodec::H264 }
        }
        None => {
            // The receiver uses this only for the virtual camera's advertised
            // rate; frames still decode at whatever size actually arrives.
            let m = media.capture_mode();
            tracing::warn!("video format not negotiated within {FORMAT_WAIT:?}; announcing the capture mode");
            VideoConfig {
                width: m.map_or(0, |m| m.width),
                height: m.map_or(0, |m| m.height),
                fps: m.map_or(0, |m| m.fps().round() as u32),
                codec: VideoCodec::H264,
            }
        }
    };
    let stream_config = ControlMessage::StreamConfig {
        video: Some(video),
        audio: Some(AudioConfig { sample_rate: 48_000, channels: AUDIO_CHANNELS, codec: AudioCodec::Opus }),
    };
    if let Err(e) = session.send_control(&stream_config).await {
        let _ = media.stop();
        return format!("sending stream config failed: {e}");
    }
    live.store(true, Ordering::Release);
    // Whatever was encoded while waiting was dropped, so open on an IDR.
    media.request_keyframe();

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
    let ctrl_session = session.clone();
    let ctrl_task = tokio::spawn(async move {
        while let Ok(Some(m)) = ctrl_session.recv_control().await {
            if ctrl_tx.send(m).is_err() {
                break;
            }
        }
    });

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let mut last = StatsSnapshot { target_kbps: abr.current(), ..Default::default() };
    let mut prev_frames = 0u64;
    let mut prev_bytes = 0u64;
    let mut prev_at = Instant::now();

    let reason = loop {
        tokio::select! {
            _ = cfg.cancel.cancelled() => {
                session.close().await;
                break "stopped".to_string();
            }
            e = session.closed() => break format!("connection closed: {e}"),
            m = ctrl_rx.recv() => match m {
                Some(ControlMessage::RequestKeyframe) => {
                    tracing::debug!("keyframe requested");
                    media.request_keyframe();
                }
                Some(ControlMessage::Stats { loss, rtt_ms }) => {
                    last.loss = loss;
                    last.rtt_ms = rtt_ms;
                    if let Some(kbps) = abr.on_report(loss) {
                        tracing::info!("loss {:.1}%: video bitrate now {kbps} kbit/s", loss * 100.0);
                        media.set_video_bitrate_kbps(kbps);
                    }
                    last.target_kbps = abr.current();
                }
                Some(ControlMessage::Bye) | None => break "receiver disconnected".to_string(),
                Some(other) => tracing::debug!("control: {other:?}"),
            },
            _ = tick.tick() => {
                if let Err(e) = media.check_error() {
                    session.close().await;
                    let msg = format!("capture failed: {e}");
                    let _ = tx.send(Event::Error(msg.clone()));
                    break msg;
                }
                let secs = prev_at.elapsed().as_secs_f32().max(0.001);
                prev_at = Instant::now();
                let frames = counters.frames.load(Ordering::Relaxed);
                let bytes = counters.bytes.load(Ordering::Relaxed);
                last.fps = (frames - prev_frames) as f32 / secs;
                last.kbps = (bytes - prev_bytes) as f32 * 8.0 / 1000.0 / secs;
                prev_frames = frames;
                prev_bytes = bytes;
                let _ = tx.send(Event::Stats(last));
            }
        }
    };
    ctrl_task.abort();
    let _ = media.stop();
    reason
}

/// Reports the microphone's peak level from the raw samples entering the Opus
/// encoder, so the sender UI can show the mic is live without decoding.
fn attach_level_meter(pipeline: &tp_media::gst::Pipeline, tx: UnboundedSender<Event>) {
    use tp_media::gst::prelude::*;
    let Some(enc) = pipeline.iterate_elements().into_iter().flatten().find(|e| {
        e.factory().is_some_and(|f| f.name() == "opusenc")
    }) else {
        return;
    };
    let Some(pad) = enc.static_pad("sink") else { return };
    let state = std::sync::Mutex::new((Instant::now(), 0f32));
    pad.add_probe(tp_media::gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(tp_media::gst::PadProbeData::Buffer(buf)) = &info.data else {
            return tp_media::gst::PadProbeReturn::Ok;
        };
        let float = pad
            .current_caps()
            .and_then(|c| c.structure(0).and_then(|s| s.get::<String>("format").ok()))
            .is_some_and(|f| f.starts_with("F32"));
        let Ok(map) = buf.map_readable() else { return tp_media::gst::PadProbeReturn::Ok };
        let peak = if float {
            map.as_slice()
                .as_chunks::<4>().0.iter()
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs())
                .fold(0f32, f32::max)
        } else {
            map.as_slice()
                .as_chunks::<2>().0.iter()
                .map(|b| (i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).abs())
                .fold(0f32, f32::max)
        };
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.1 = st.1.max(peak);
        if st.0.elapsed() >= Duration::from_millis(50) {
            let _ = tx.send(Event::AudioLevel(st.1.min(1.0)));
            *st = (Instant::now(), 0.0);
        }
        tp_media::gst::PadProbeReturn::Ok
    });
}

async fn fail(session: &Session, tx: &UnboundedSender<Event>, msg: String) -> String {
    tracing::error!("{msg}");
    let _ = tx.send(Event::Error(msg.clone()));
    session.close().await;
    msg
}
