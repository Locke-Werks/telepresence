//! The QObject QML talks to. Core tasks run on the shared tokio runtime and
//! report through an event channel; a pump task turns those events into
//! property updates queued onto the Qt thread.

// QML calls start_sender with every picker's value at once.
#![allow(clippy::too_many_arguments)]
#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("video_sink.h");
        type QVideoSink;

        /// # Safety
        /// The sink must be null or point to a live QVideoSink; call on the Qt thread.
        #[namespace = "tp"]
        unsafe fn set_video_sink(sink: *mut QVideoSink);
        #[namespace = "tp"]
        fn push_nv12(width: u32, height: u32, stride: u32, data: &[u8], uv_offset: usize);
        #[namespace = "tp"]
        fn clear_video();
    }

    #[auto_cxx_name]
    extern "RustQt" {
        #[qobject]
        #[qml_element]
        /// "receiver" or "sender": what the running (or next) session is.
        #[qproperty(QString, role)]
        #[qproperty(bool, running)]
        #[qproperty(QString, status)]
        /// Connected peer as "name (addr)", empty when idle.
        #[qproperty(QString, peer)]
        #[qproperty(QString, pairing_code)]
        #[qproperty(bool, needs_code)]
        #[qproperty(f64, fps)]
        #[qproperty(f64, kbps)]
        #[qproperty(f64, loss)]
        #[qproperty(i32, rtt)]
        #[qproperty(i32, target_kbps)]
        #[qproperty(f64, level)]
        /// JSON `[{id, name, addr, fingerprint, paired, pinned}]` of receivers
        /// seen on the LAN. `pinned` is whether this sender has paired with it.
        #[qproperty(QString, peers)]
        /// JSON `[{id, name}]`. Microphones never include a "Default" entry.
        #[qproperty(QString, cameras)]
        #[qproperty(QString, mics)]
        /// JSON array of audio output names.
        #[qproperty(QString, outputs)]
        /// JSON `[{label, w, h, fps}]` of the selected camera's native modes.
        #[qproperty(QString, modes)]
        /// JSON `[{fingerprint, name}]` of senders this receiver has paired.
        #[qproperty(QString, paired)]
        /// JSON of the saved settings: `{role, platform, sender: {...}, receiver: {...}}`.
        #[qproperty(QString, saved)]
        /// Negotiated video as "WxH @ N fps", empty when idle.
        #[qproperty(QString, video_format)]
        #[qproperty(QString, error)]
        /// Updater phase: "" (nothing to show), "checking", "uptodate",
        /// "available", "downloading", "waiting" (held for a live stream),
        /// "applying", "manual" (newer version, install it by hand),
        /// "notice" or "error".
        #[qproperty(QString, update_state)]
        #[qproperty(QString, update_version)]
        /// 0..1 while downloading; -1 when the size is unknown.
        #[qproperty(f64, update_progress)]
        #[qproperty(QString, update_message)]
        /// Release page, for installs that cannot update themselves.
        #[qproperty(QString, update_url)]
        #[qproperty(bool, auto_update_check)]
        type Controller = super::ControllerRust;

        /// Asks GitHub now, regardless of when the last check ran.
        #[qinvokable]
        fn check_for_updates(self: Pin<&mut Controller>);

        /// Downloads, verifies and applies the offered version, or arms it to
        /// do so when the current stream ends.
        #[qinvokable]
        fn install_update(self: Pin<&mut Controller>);

        /// Hides the banner until the next check.
        #[qinvokable]
        fn later_update(self: Pin<&mut Controller>);

        /// Stops automatic checks from offering this version again.
        #[qinvokable]
        fn skip_update(self: Pin<&mut Controller>);

        #[qinvokable]
        fn set_auto_update(self: Pin<&mut Controller>, on: bool);

        /// Starts device enumeration and LAN browsing. Called once from QML.
        #[qinvokable]
        fn init(self: Pin<&mut Controller>);

        #[qinvokable]
        fn refresh_devices(self: Pin<&mut Controller>);

        /// Lists native modes for a camera id into `modes`.
        #[qinvokable]
        fn load_modes(self: Pin<&mut Controller>, camera: &QString);

        /// Remembers which role the window opens in next time.
        #[qinvokable]
        fn set_saved_role(self: Pin<&mut Controller>, role: &QString);

        #[qinvokable]
        fn forget_sender(self: Pin<&mut Controller>, fingerprint: &QString);

        /// Saves name, port and audio output, then (re)starts the receiver.
        #[qinvokable]
        fn start_receiver(self: Pin<&mut Controller>, port: i32, name: &QString, audio_out: &QString);

        /// Saves the choices, then (re)starts the sender. Empty `camera`/`mic`
        /// keeps the saved device (or picks one); `mode` is "native" or
        /// "WxH@fps"; `bitrate` 0 is automatic.
        #[qinvokable]
        fn start_sender(
            self: Pin<&mut Controller>,
            peer: &QString,
            code: &QString,
            camera: &QString,
            mic: &QString,
            mode: &QString,
            bitrate: i32,
            test_source: bool,
        );

        #[qinvokable]
        fn stop(self: Pin<&mut Controller>);

        /// Receiver video goes to this sink, the one behind the QML VideoOutput.
        ///
        /// # Safety
        /// The sink must be null or a QVideoSink that outlives the controller.
        #[qinvokable]
        unsafe fn attach_video_sink(self: Pin<&mut Controller>, sink: *mut QVideoSink);
    }

    impl cxx_qt::Threading for Controller {}
}

use std::pin::Pin;
use std::sync::Arc;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use crate::{Event, PeerInfo, ShmSink, ReceiverConfig, SenderConfig};

pub struct ControllerRust {
    role: QString,
    running: bool,
    status: QString,
    peer: QString,
    pairing_code: QString,
    needs_code: bool,
    fps: f64,
    kbps: f64,
    loss: f64,
    rtt: i32,
    target_kbps: i32,
    level: f64,
    peers: QString,
    cameras: QString,
    mics: QString,
    outputs: QString,
    modes: QString,
    paired: QString,
    saved: QString,
    video_format: QString,
    error: QString,
    /// Bumped on every start and stop, so events from a session that has
    /// already been stopped cannot overwrite the next one's state.
    generation: u64,
    cancel: Option<CancellationToken>,
    browser: Option<CancellationToken>,
    seen: Vec<PeerInfo>,
    update_state: QString,
    update_version: QString,
    update_progress: f64,
    update_message: QString,
    update_url: QString,
    auto_update_check: bool,
    updates: Updates,
}

#[derive(Default)]
struct Updates {
    install: Option<crate::update::InstallKind>,
    release: Option<crate::update::Release>,
    package: Option<crate::update::Package>,
    /// A verified download waiting to be applied.
    file: Option<std::path::PathBuf>,
    gate: crate::update::ApplyGate,
    busy: bool,
}

impl Default for ControllerRust {
    fn default() -> Self {
        Self {
            role: QString::from("receiver"),
            running: false,
            status: QString::from("Idle"),
            peer: QString::default(),
            pairing_code: QString::default(),
            needs_code: false,
            fps: 0.0,
            kbps: 0.0,
            loss: 0.0,
            rtt: 0,
            target_kbps: 0,
            level: 0.0,
            peers: QString::from("[]"),
            cameras: QString::from("[]"),
            mics: QString::from("[]"),
            outputs: QString::from("[]"),
            modes: QString::from("[]"),
            paired: QString::from("[]"),
            saved: QString::from("{}"),
            video_format: QString::default(),
            error: QString::default(),
            generation: 0,
            cancel: None,
            browser: None,
            seen: Vec::new(),
            update_state: QString::default(),
            update_version: QString::default(),
            update_progress: 0.0,
            update_message: QString::default(),
            update_url: QString::default(),
            auto_update_check: true,
            updates: Updates::default(),
        }
    }
}

impl Drop for ControllerRust {
    fn drop(&mut self) {
        for c in [self.cancel.take(), self.browser.take()].into_iter().flatten() {
            c.cancel();
        }
    }
}

fn qs(s: &str) -> QString {
    QString::from(s)
}

/// Feeds the receiver's decoded frames to the QML VideoOutput.
struct GuiVideo;

impl crate::FrameSink for GuiVideo {
    fn video(&self, f: &crate::VideoFrame<'_>) {
        qobject::push_nv12(f.width, f.height, f.stride, f.data, f.uv_offset);
    }

    fn audio(&self, _block: &crate::AudioBlock<'_>) {}
}

impl qobject::Controller {
    pub fn init(mut self: Pin<&mut Self>) {
        if self.browser.is_some() {
            return;
        }
        self.as_mut().refresh_devices();
        self.as_mut().publish_saved();
        self.as_mut().publish_paired();
        let cancel = CancellationToken::new();
        self.as_mut().rust_mut().browser = Some(cancel.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        crate::sender::spawn_browser(tx, cancel);
        // Generation 0 is never a session generation, so these are not filtered.
        self.as_mut().pump(rx, None);
        self.startup_update_check();
    }

    pub fn refresh_devices(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        crate::gui::runtime().spawn_blocking(move || {
            use tp_media::DeviceKind;
            let list = tp_media::list_devices().unwrap_or_else(|e| {
                tracing::warn!("device enumeration failed: {e}");
                Vec::new()
            });
            let pick = |k| {
                let v: Vec<_> = crate::devices::pickable(&list, k)
                    .into_iter()
                    .map(|(id, name)| serde_json::json!({"id": id, "name": name}))
                    .collect();
                serde_json::Value::Array(v).to_string()
            };
            let (cams, mics) = (pick(DeviceKind::Camera), pick(DeviceKind::Microphone));
            let outs: Vec<_> =
                list.iter().filter(|d| d.kind == DeviceKind::AudioOutput).map(|d| d.name.clone()).collect();
            let outs = serde_json::to_string(&outs).unwrap_or_default();
            let _ = qt.queue(move |mut c| {
                c.as_mut().set_cameras(qs(&cams));
                c.as_mut().set_mics(qs(&mics));
                c.as_mut().set_outputs(qs(&outs));
            });
        });
    }

    pub fn load_modes(self: Pin<&mut Self>, camera: &QString) {
        let key = camera.to_string();
        let qt = self.qt_thread();
        crate::gui::runtime().spawn_blocking(move || {
            let modes = if key.is_empty() {
                Vec::new()
            } else {
                crate::devices::camera_modes(&key).unwrap_or_else(|e| {
                    tracing::warn!("reading modes of {key}: {e:#}");
                    Vec::new()
                })
            };
            let v: Vec<_> = modes
                .iter()
                .map(|m| {
                    let fps = m.fps().round() as u32;
                    serde_json::json!({
                        "label": format!("{}x{} @ {} fps", m.width, m.height, fps),
                        "w": m.width, "h": m.height, "fps": fps,
                    })
                })
                .collect();
            let json = serde_json::Value::Array(v).to_string();
            let _ = qt.queue(move |mut c| c.as_mut().set_modes(qs(&json)));
        });
    }

    pub fn set_saved_role(mut self: Pin<&mut Self>, role: &QString) {
        let dir = crate::gui::config_dir();
        let res = crate::store::AppSettings::load(&dir).and_then(|mut s| {
            s.role = Some(role.to_string());
            s.save(&dir)
        });
        if let Err(e) = res {
            self.as_mut().set_error(qs(&format!("saving settings: {e:#}")));
        }
        self.publish_saved();
    }

    pub fn forget_sender(mut self: Pin<&mut Self>, fingerprint: &QString) {
        let dir = crate::store::role_dir(&crate::gui::config_dir(), "receiver");
        let res = crate::store::JsonPeerStore::open(dir.join("peers.json"))
            .and_then(|s| s.forget(&fingerprint.to_string()));
        if let Err(e) = res {
            self.as_mut().set_error(qs(&format!("forgetting sender: {e:#}")));
        }
        self.publish_paired();
    }

    pub fn start_receiver(mut self: Pin<&mut Self>, port: i32, name: &QString, audio_out: &QString) {
        let name = name.to_string().trim().to_string();
        let port = u16::try_from(port).unwrap_or(crate::DEFAULT_PORT);
        let audio_out = audio_out.to_string();
        let audio_out = if audio_out.trim().is_empty() { "auto".to_string() } else { audio_out };
        let base = crate::gui::config_dir();
        let saved = crate::store::ReceiverSettings {
            name: (!name.is_empty()).then(|| name.clone()),
            port: Some(port),
            audio_out: Some(audio_out.clone()),
        };
        let role_saved = crate::store::AppSettings { role: Some("receiver".into()) };
        if let Err(e) = saved.save(&crate::store::role_dir(&base, "receiver")).and_then(|_| role_saved.save(&base)) {
            tracing::warn!("saving receiver settings: {e:#}");
        }
        self.as_mut().publish_saved();
        let (tx, rx, cancel) = self.as_mut().begin("receiver");
        let cfg = ReceiverConfig {
            port,
            name: if name.is_empty() { crate::default_name() } else { name },
            audio_out: audio_out.parse().unwrap_or(crate::AudioOut::Auto),
            config_dir: crate::gui::config_dir(),
            sink: Arc::new(ShmSink::new()),
            advertise: true,
            preview: Some(Arc::new(GuiVideo)),
            cancel,
        };
        let gen_ = self.generation;
        let qt = self.qt_thread();
        crate::gui::runtime().spawn(async move {
            let res = crate::run_receiver(cfg, tx).await;
            finish(qt, gen_, res);
        });
        self.pump(rx, Some(gen_));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_sender(
        mut self: Pin<&mut Self>,
        peer: &QString,
        code: &QString,
        camera: &QString,
        mic: &QString,
        mode: &QString,
        bitrate: i32,
        test_source: bool,
    ) {
        let opt = |s: &QString| {
            let s = s.to_string().trim().to_string();
            (!s.is_empty()).then_some(s)
        };
        let (peer, code, camera, mic) = (opt(peer), opt(code), opt(camera), opt(mic));
        let (size, fps) = parse_mode(&mode.to_string());
        let base = crate::gui::config_dir();
        let dir = crate::store::role_dir(&base, "sender");
        // The picks go to the same settings.json the CLI reads; run_sender
        // then resolves everything from there.
        let res = crate::store::SenderSettings::load(&dir).and_then(|mut s| {
            if camera.is_some() {
                s.camera = camera.clone();
            }
            if mic.is_some() {
                s.mic = mic.clone();
            }
            s.size = size;
            s.fps = fps;
            s.bitrate_kbps = u32::try_from(bitrate).ok().filter(|b| *b > 0);
            s.save(&dir)?;
            crate::store::AppSettings { role: Some("sender".into()) }.save(&base)
        });
        if let Err(e) = res {
            tracing::warn!("saving sender settings: {e:#}");
        }
        self.as_mut().publish_saved();
        let (tx, rx, cancel) = self.as_mut().begin("sender");
        let cfg = SenderConfig {
            peer,
            code,
            camera,
            mic,
            test_source,
            size: None,
            fps: None,
            bitrate_kbps: None,
            name: crate::default_name(),
            config_dir: crate::gui::config_dir(),
            cancel,
        };
        let gen_ = self.generation;
        let qt = self.qt_thread();
        crate::gui::runtime().spawn(async move {
            let res = crate::run_sender(cfg, tx).await;
            finish(qt, gen_, res);
        });
        self.pump(rx, Some(gen_));
    }

    pub unsafe fn attach_video_sink(self: Pin<&mut Self>, sink: *mut qobject::QVideoSink) {
        // SAFETY: the caller guarantees the pointer; QML hands over the sink of
        // a VideoOutput that lives as long as the window, and so the controller.
        unsafe { qobject::set_video_sink(sink) };
    }

    pub fn stop(mut self: Pin<&mut Self>) {
        if let Some(c) = self.as_mut().rust_mut().cancel.take() {
            c.cancel();
        }
        self.as_mut().rust_mut().generation += 1;
        self.as_mut().set_running(false);
        self.as_mut().reset_session_view();
        self.as_mut().set_status(qs("Idle"));
    }

    /// Stops whatever runs, then prepares state for a new session of `role`.
    fn begin(
        mut self: Pin<&mut Self>,
        role: &str,
    ) -> (mpsc::UnboundedSender<Event>, mpsc::UnboundedReceiver<Event>, CancellationToken) {
        self.as_mut().stop();
        let cancel = CancellationToken::new();
        self.as_mut().rust_mut().cancel = Some(cancel.clone());
        self.as_mut().set_role(qs(role));
        self.as_mut().set_running(true);
        self.as_mut().set_needs_code(false);
        self.as_mut().set_error(QString::default());
        self.as_mut().set_status(qs("Starting"));
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, rx, cancel)
    }

    fn reset_session_view(mut self: Pin<&mut Self>) {
        self.as_mut().set_peer(QString::default());
        self.as_mut().set_pairing_code(QString::default());
        self.as_mut().set_fps(0.0);
        self.as_mut().set_kbps(0.0);
        self.as_mut().set_loss(0.0);
        self.as_mut().set_rtt(0);
        self.as_mut().set_target_kbps(0);
        self.as_mut().set_level(0.0);
        qobject::clear_video();
        self.as_mut().set_video_format(QString::default());
        // Every way a stream ends passes through here.
        self.as_mut().update_stream_changed();
    }

    fn publish_saved(mut self: Pin<&mut Self>) {
        let base = crate::gui::config_dir();
        let app = crate::store::AppSettings::load(&base).unwrap_or_default();
        let snd = crate::store::SenderSettings::load(&crate::store::role_dir(&base, "sender")).unwrap_or_default();
        let rcv = crate::store::ReceiverSettings::load(&crate::store::role_dir(&base, "receiver")).unwrap_or_default();
        let json = serde_json::json!({
            "role": app.role,
            "platform": std::env::consts::OS,
            "sender": snd,
            "receiver": rcv,
        });
        self.as_mut().set_saved(qs(&json.to_string()));
    }

    fn publish_paired(mut self: Pin<&mut Self>) {
        let dir = crate::store::role_dir(&crate::gui::config_dir(), "receiver");
        let list = crate::store::JsonPeerStore::open(dir.join("peers.json")).map(|s| s.list()).unwrap_or_default();
        let v: Vec<_> =
            list.iter().map(|p| serde_json::json!({"fingerprint": p.fingerprint, "name": p.name})).collect();
        self.as_mut().set_paired(qs(&serde_json::Value::Array(v).to_string()));
    }

    /// Forwards events to the Qt thread. `generation` is `None` for the
    /// browser, whose events are valid regardless of sessions.
    fn pump(self: Pin<&mut Self>, mut rx: mpsc::UnboundedReceiver<Event>, generation: Option<u64>) {
        let qt = self.qt_thread();
        crate::gui::runtime().spawn(async move {
            while let Some(ev) = rx.recv().await {
                let ok = qt.queue(move |c| {
                    if generation.is_none_or(|g| g == c.generation) {
                        c.apply(ev);
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        });
    }

    fn apply(mut self: Pin<&mut Self>, ev: Event) {
        match ev {
            Event::Status(s) => self.set_status(qs(&s)),
            Event::Listening { port, .. } => self.set_status(qs(&format!("Listening on UDP {port}"))),
            Event::PairingCode(code) => self.set_pairing_code(qs(code.as_deref().unwrap_or(""))),
            Event::PeerFound(p) => {
                let seen = &mut self.as_mut().rust_mut().seen;
                match seen.iter_mut().find(|s| s.id == p.id) {
                    Some(s) => *s = p,
                    None => seen.push(p),
                }
                self.publish_peers();
            }
            Event::PeerLost(id) => {
                self.as_mut().rust_mut().seen.retain(|s| s.id != id);
                self.publish_peers();
            }
            Event::Connected { peer, addr } => {
                self.as_mut().set_peer(qs(&format!("{peer} ({addr})")));
                self.as_mut().set_needs_code(false);
                self.as_mut().set_error(QString::default());
                // Both roles pin on a successful pairing; show it at once.
                self.as_mut().publish_paired();
                self.as_mut().publish_peers();
            }
            Event::VideoFormat { width, height, fps } => {
                let f = if fps > 0 { format!("{width}x{height} @ {fps} fps") } else { format!("{width}x{height}") };
                self.set_video_format(qs(&f));
            }
            Event::Disconnected { reason } => {
                self.as_mut().reset_session_view();
                self.as_mut().set_status(qs(&format!("Disconnected: {reason}")));
            }
            Event::NeedsPairingCode => {
                self.as_mut().set_needs_code(true);
                // The sender may just have dropped a stale pin.
                self.publish_peers();
            }
            Event::Stats(s) => {
                self.as_mut().set_fps(s.fps as f64);
                self.as_mut().set_kbps(s.kbps as f64);
                self.as_mut().set_loss(s.loss as f64);
                self.as_mut().set_rtt(s.rtt_ms as i32);
                self.as_mut().set_target_kbps(s.target_kbps as i32);
            }
            Event::AudioLevel(l) => self.set_level(l as f64),
            Event::Error(e) => self.set_error(qs(&e)),
        }
    }

    fn publish_peers(mut self: Pin<&mut Self>) {
        let dir = crate::store::role_dir(&crate::gui::config_dir(), "sender");
        let pins: std::collections::HashSet<String> = crate::store::JsonPeerStore::open(dir.join("peers.json"))
            .map(|s| s.list().into_iter().map(|p| p.fingerprint).collect())
            .unwrap_or_default();
        let list: Vec<_> = self
            .seen
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id, "name": p.name, "addr": p.addr, "fingerprint": p.fingerprint,
                    "paired": p.paired, "pinned": pins.contains(&p.fingerprint.to_ascii_lowercase()),
                })
            })
            .collect();
        let json = serde_json::Value::Array(list).to_string();
        self.as_mut().set_peers(qs(&json));
    }
}

fn finish(qt: cxx_qt::CxxQtThread<qobject::Controller>, generation: u64, res: anyhow::Result<()>) {
    let _ = qt.queue(move |mut c| {
        if c.generation != generation {
            return;
        }
        c.as_mut().set_running(false);
        c.as_mut().reset_session_view();
        match res {
            Ok(()) => c.as_mut().set_status(qs("Stopped")),
            Err(e) => {
                c.as_mut().set_status(qs("Stopped"));
                c.as_mut().set_error(qs(&format!("{e:#}")));
            }
        }
    });
}

// ----------------------------------------------------------------- updates

use crate::update::{self, Applied, CheckResult, GateAction, UpdateSettings};

impl qobject::Controller {
    fn startup_update_check(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().updates.install = Some(update::detect_install());
        let s = UpdateSettings::load(&crate::gui::config_dir());
        self.as_mut().set_auto_update_check(s.auto_check);
        if s.due(update::now_secs()) {
            self.run_check(false);
        }
    }

    pub fn check_for_updates(self: Pin<&mut Self>) {
        self.run_check(true);
    }

    pub fn set_auto_update(mut self: Pin<&mut Self>, on: bool) {
        let dir = crate::gui::config_dir();
        let mut s = UpdateSettings::load(&dir);
        s.auto_check = on;
        if let Err(e) = s.save(&dir) {
            tracing::warn!("saving update settings: {e:#}");
        }
        self.as_mut().set_auto_update_check(on);
    }

    pub fn later_update(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().updates.gate.cancel();
        if !self.updates.busy {
            self.as_mut().set_update_state(QString::default());
        }
    }

    pub fn skip_update(self: Pin<&mut Self>) {
        let dir = crate::gui::config_dir();
        let mut s = UpdateSettings::load(&dir);
        s.skipped = Some(self.update_version.to_string());
        if let Err(e) = s.save(&dir) {
            tracing::warn!("saving update settings: {e:#}");
        }
        self.later_update();
    }

    /// A peer is connected: the one state an update must never interrupt.
    fn streaming(&self) -> bool {
        !self.peer.is_empty()
    }

    fn run_check(mut self: Pin<&mut Self>, manual: bool) {
        if self.updates.busy || self.updates.gate.armed() {
            return;
        }
        if manual {
            self.as_mut().set_update_state(qs("checking"));
            self.as_mut().set_update_message(qs("Checking for updates"));
        }
        let qt = self.qt_thread();
        let dir = crate::gui::config_dir();
        crate::gui::runtime().spawn(async move {
            let mut st = UpdateSettings::load(&dir);
            let res = match update::Updater::from_env() {
                Ok(u) => u.check(&mut st, &update::current_version()).await,
                Err(e) => Err(e),
            };
            // Reload before saving so a toggle made during the request survives.
            let mut fresh = UpdateSettings::load(&dir);
            (fresh.etag, fresh.cached, fresh.last_check) = (st.etag, st.cached, st.last_check);
            if let Err(e) = fresh.save(&dir) {
                tracing::warn!("saving update settings: {e:#}");
            }
            let skipped = fresh.skipped;
            let _ = qt.queue(move |c| c.check_done(res, manual, skipped));
        });
    }

    fn check_done(mut self: Pin<&mut Self>, res: anyhow::Result<CheckResult>, manual: bool, skipped: Option<String>) {
        let cur = update::current_version();
        match res {
            Ok(CheckResult::Available(r)) => {
                let ver = r.version.to_string();
                if !manual && skipped.as_deref() == Some(ver.as_str()) {
                    self.as_mut().set_update_state(QString::default());
                    return;
                }
                let install = self.updates.install.clone().unwrap_or(update::InstallKind::Unknown);
                let writable = match &install {
                    update::InstallKind::MacBundle(b) => update::bundle_writable(b),
                    _ => true,
                };
                let arch = std::env::consts::ARCH;
                let package = update::package_for(&install, writable);
                let asset = package.and_then(|p| update::select_asset(&r, p, arch)).map(|a| a.name.clone());
                let applies = install.can_apply() && asset.is_some();
                let msg = match (&install, &asset) {
                    _ if applies => String::new(),
                    (update::InstallKind::Dev, _) => "Development build: updates are shown, not installed.".into(),
                    (update::InstallKind::Deb, Some(a)) => {
                        format!("Installed from a package: download {a} from the release page and install it.")
                    }
                    _ => "Download it from the release page.".into(),
                };
                self.as_mut().set_update_version(qs(&ver));
                self.as_mut().set_update_url(qs(&r.html_url));
                self.as_mut().set_update_message(qs(&msg));
                let u = &mut self.as_mut().rust_mut().updates;
                // A new release replaces anything downloaded for an older one.
                if u.release.as_ref().is_none_or(|old| old.version != r.version) {
                    u.file = None;
                }
                u.package = package;
                u.release = Some(r);
                self.as_mut().set_update_state(qs(if applies { "available" } else { "manual" }));
            }
            Ok(CheckResult::UpToDate) => {
                let state = if manual { "uptodate" } else { "" };
                self.as_mut().set_update_message(qs(&format!("TelePresence {cur} is up to date")));
                self.as_mut().set_update_state(qs(state));
            }
            Ok(CheckResult::RateLimited { reset }) => {
                tracing::info!("update check rate limited until {reset:?}");
                if manual {
                    let mins = reset.map(|r| r.saturating_sub(update::now_secs()).div_ceil(60).max(1));
                    let when = mins.map(|m| format!(" in {m} min")).unwrap_or_else(|| " later".into());
                    self.as_mut()
                        .set_update_message(qs(&format!("GitHub is limiting update checks. Try again{when}.")));
                    self.as_mut().set_update_state(qs("notice"));
                } else {
                    self.as_mut().set_update_state(QString::default());
                }
            }
            Err(e) => {
                tracing::warn!("update check failed: {e:#}");
                if manual {
                    self.as_mut().set_update_message(qs(&format!("Update check failed: {e:#}")));
                    self.as_mut().set_update_state(qs("error"));
                } else {
                    self.as_mut().set_update_state(QString::default());
                }
            }
        }
    }

    pub fn install_update(mut self: Pin<&mut Self>) {
        if self.updates.busy || self.updates.release.is_none() {
            return;
        }
        let streaming = self.streaming();
        match self.as_mut().rust_mut().updates.gate.request(streaming) {
            GateAction::Wait => self.as_mut().set_waiting(),
            GateAction::Proceed => self.proceed_update(),
        }
    }

    fn set_waiting(mut self: Pin<&mut Self>) {
        self.as_mut().set_update_message(qs("Installs when the stream ends."));
        self.as_mut().set_update_state(qs("waiting"));
    }

    fn update_stream_changed(mut self: Pin<&mut Self>) {
        let streaming = self.streaming();
        if self.as_mut().rust_mut().updates.gate.stream_changed(streaming) == GateAction::Proceed {
            self.proceed_update();
        }
    }

    /// Applies a download already on hand, or fetches one first.
    fn proceed_update(mut self: Pin<&mut Self>) {
        if let Some(file) = self.updates.file.clone() {
            self.apply_update(file);
            return;
        }
        let u = &self.updates;
        let (Some(rel), Some(pkg)) = (u.release.clone(), u.package) else { return };
        let Some(asset) = update::select_asset(&rel, pkg, std::env::consts::ARCH).cloned() else { return };
        self.as_mut().rust_mut().updates.busy = true;
        self.as_mut().set_update_progress(0.0);
        self.as_mut().set_update_message(qs(&format!("Downloading {}", asset.name)));
        self.as_mut().set_update_state(qs("downloading"));
        let qt = self.qt_thread();
        crate::gui::runtime().spawn(async move {
            let dir = update::download_dir(&rel.version);
            let qt_p = qt.clone();
            let mut last = -1i64;
            let res = match update::Updater::from_env() {
                Ok(u) => {
                    u.download(&rel, &asset, &dir, |got, total| {
                        // Queue at most one update per percent, not one per chunk.
                        let frac = if total > 0 { got as f64 / total as f64 } else { -1.0 };
                        let step = (frac * 100.0) as i64;
                        if step != last {
                            last = step;
                            let _ = qt_p.queue(move |c| c.set_update_progress(frac));
                        }
                    })
                    .await
                }
                Err(e) => Err(e),
            };
            let _ = qt.queue(move |c| c.download_done(res));
        });
    }

    fn download_done(mut self: Pin<&mut Self>, res: anyhow::Result<std::path::PathBuf>) {
        self.as_mut().rust_mut().updates.busy = false;
        match res {
            Ok(file) => {
                self.as_mut().rust_mut().updates.file = Some(file.clone());
                // A stream may have started while the file came down.
                let streaming = self.streaming();
                match self.as_mut().rust_mut().updates.gate.request(streaming) {
                    GateAction::Wait => self.set_waiting(),
                    GateAction::Proceed => self.apply_update(file),
                }
            }
            Err(e) => {
                tracing::warn!("update download failed: {e:#}");
                self.as_mut().set_update_message(qs(&format!("Update failed: {e:#}")));
                self.as_mut().set_update_state(qs("error"));
            }
        }
    }

    fn apply_update(mut self: Pin<&mut Self>, file: std::path::PathBuf) {
        let Some(install) = self.updates.install.clone() else { return };
        self.as_mut().set_update_message(qs("Installing"));
        self.as_mut().set_update_state(qs("applying"));
        match update::apply(&install, &file) {
            Ok(Applied::Exit) => {
                tracing::info!("update handed off, exiting");
                self.as_mut().stop();
                if let Some(b) = self.as_mut().rust_mut().browser.take() {
                    b.cancel();
                }
                // Give the cancelled tasks a moment to release the virtual
                // devices before the process goes.
                crate::gui::runtime().spawn(async {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    std::process::exit(0);
                });
            }
            Ok(Applied::Handoff(msg)) => {
                self.as_mut().set_update_message(qs(&msg));
                self.as_mut().set_update_state(qs("manual"));
            }
            Err(e) => {
                tracing::warn!("applying update failed: {e:#}");
                self.as_mut().set_update_message(qs(&format!("Update failed: {e:#}")));
                self.as_mut().set_update_state(qs("error"));
            }
        }
    }
}

/// "native" (or anything unparsable) is (None, None); "1920x1080@30" pins both.
fn parse_mode(m: &str) -> (Option<(u32, u32)>, Option<u32>) {
    let (dims, fps) = m.split_once('@').unwrap_or((m, ""));
    let size = dims.split_once('x').and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
    match size {
        Some(s) => (Some(s), fps.trim().parse().ok()),
        None => (None, None),
    }
}
