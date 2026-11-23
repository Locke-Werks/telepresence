//! Receiver and sender in one process over loopback QUIC: pairing with the
//! displayed code, then real encode, transport and decode with test sources.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use std::sync::atomic::{AtomicU64, Ordering};

use tp_app::{AudioBlock, AudioOut, Event, FrameSink, NullSink, ReceiverConfig, SenderConfig, VideoFrame};

/// Counts full-size frames reaching the preview tap.
#[derive(Default)]
struct CountingSink {
    frames: AtomicU64,
    width: AtomicU64,
}

impl FrameSink for CountingSink {
    fn video(&self, f: &VideoFrame<'_>) {
        self.width.store(f.width as u64, Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
    }
    fn audio(&self, _: &AudioBlock<'_>) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairs_and_streams_over_loopback() {
    let dir = std::env::temp_dir().join(format!("tp-app-loopback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cancel = CancellationToken::new();
    let preview = Arc::new(CountingSink::default());

    let (rtx, mut rrx) = mpsc::unbounded_channel();
    let receiver = tokio::spawn(tp_app::run_receiver(
        ReceiverConfig {
            port: 0,
            name: "test-receiver".into(),
            // Keep the test off real audio devices.
            audio_out: AudioOut::None,
            config_dir: dir.clone(),
            sink: Arc::new(NullSink),
            advertise: false,
            preview: Some(preview.clone()),
            cancel: cancel.clone(),
        },
        rtx,
    ));

    let (mut port, mut code) = (None, None);
    while port.is_none() || code.is_none() {
        match tokio::time::timeout(Duration::from_secs(10), rrx.recv()).await.expect("receiver start").unwrap() {
            Event::Listening { port: p, .. } => port = Some(p),
            Event::PairingCode(Some(c)) => code = Some(c),
            _ => {}
        }
    }

    let (stx, mut srx) = mpsc::unbounded_channel();
    let sender = tokio::spawn(tp_app::run_sender(
        SenderConfig {
            peer: Some(format!("127.0.0.1:{}", port.unwrap())),
            code,
            camera: None,
            mic: None,
            test_source: true,
            size: Some((640, 360)),
            fps: Some(30),
            bitrate_kbps: Some(2000),
            name: "test-sender".into(),
            config_dir: dir.clone(),
            cancel: cancel.clone(),
        },
        stx,
    ));
    tokio::spawn(async move { while srx.recv().await.is_some() {} });

    let (mut fps, mut level) = (0f32, 0f32);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while (fps < 10.0 || level <= 0.0) && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, rrx.recv()).await {
            Ok(Some(Event::Stats(s))) => fps = fps.max(s.fps),
            Ok(Some(Event::AudioLevel(l))) => level = level.max(l),
            Ok(Some(Event::Error(e))) => panic!("receiver error: {e}"),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), sender).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), receiver).await;
    let _ = std::fs::remove_dir_all(&dir);

    assert!(fps >= 10.0, "decoded fps too low: {fps}");
    assert!(level > 0.0, "no decoded audio");
    // The tap sees every decoded frame at the sent size, not a thumbnail.
    assert!(preview.frames.load(Ordering::Relaxed) >= 10, "preview tap got too few frames");
    assert_eq!(preview.width.load(Ordering::Relaxed), 640);
}
