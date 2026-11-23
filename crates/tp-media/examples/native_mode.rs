//! Prints each camera's advertised caps and the mode the sender would pin,
//! then encodes a 1920x1080@30 test pattern for a few seconds and reports the
//! bitrate the encoder actually produced against its target.
//!
//! cargo run -p tp-media --example native_mode [-- seconds]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tp_media::gst::prelude::*;
use tp_media::*;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    init()?;
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let cams: Vec<_> = list_devices()?
        .into_iter()
        .filter(|d| d.kind == DeviceKind::Camera)
        .collect();
    if cams.is_empty() {
        println!("no cameras found");
    }
    for cam in &cams {
        println!("camera: {} (api {:?})", cam.name, cam.api);
        match cam.device.caps() {
            Some(c) => {
                for (s, f) in c.iter_with_features() {
                    println!("  {s} [{f}]");
                }
                match choose_mode(&c, ModeOverride::default()) {
                    Some(m) => println!(
                        "  chosen native mode: {m}, default bitrate {} kbit/s",
                        default_bitrate_kbps(m.width, m.height, m.fps())
                    ),
                    None => println!("  no usable mode in device caps"),
                }
            }
            None => println!("  device reports no caps"),
        }
    }

    let bytes = Arc::new(AtomicU64::new(0));
    let frames = Arc::new(AtomicU64::new(0));
    let cfg = SenderConfig {
        video: Some(VideoSource::TestPattern),
        audio: None,
        video_size: Some((1920, 1080)),
        video_fps: Some(30),
        ..Default::default()
    };
    let sender = Sender::new(
        cfg,
        {
            let bytes = bytes.clone();
            let frames = frames.clone();
            Box::new(move |v: EncodedVideo| {
                bytes.fetch_add(v.data.len() as u64, Ordering::Relaxed);
                frames.fetch_add(1, Ordering::Relaxed);
            })
        },
        Box::new(|_| {}),
    )?;
    // A static pattern compresses to almost nothing and would say nothing
    // about rate control, so give the encoder motion and detail to spend on.
    for el in sender.pipeline().iterate_elements().into_iter().flatten() {
        if el
            .factory()
            .map(|f| f.name() == "videotestsrc")
            .unwrap_or(false)
        {
            let pattern = std::env::var("TP_PATTERN").unwrap_or_else(|_| "smpte".into());
            el.set_property_from_str("pattern", &pattern);
            el.set_property("horizontal-speed", 8i32);
        }
    }
    let target = sender.initial_video_bitrate_kbps();
    println!(
        "encoder {:?}, target {target} kbit/s",
        sender.encoder().map(|e| e.factory_name())
    );
    sender.start()?;

    // Skip the first second: rate control settles after the opening IDR.
    std::thread::sleep(Duration::from_secs(1));
    sender.check_error()?;
    let (b0, f0, t0) = (
        bytes.load(Ordering::Relaxed),
        frames.load(Ordering::Relaxed),
        Instant::now(),
    );
    std::thread::sleep(Duration::from_secs(secs));
    sender.check_error()?;
    let dt = t0.elapsed().as_secs_f64();
    let kbps = (bytes.load(Ordering::Relaxed) - b0) as f64 * 8.0 / dt / 1000.0;
    let fps = (frames.load(Ordering::Relaxed) - f0) as f64 / dt;
    println!(
        "format {:?}; measured {kbps:.0} kbit/s at {fps:.1} fps over {dt:.1} s ({:.0}% of target)",
        sender.video_format(),
        kbps / target as f64 * 100.0
    );

    let half = target / 2;
    sender.set_video_bitrate_kbps(half);
    std::thread::sleep(Duration::from_secs(1));
    let (b1, t1) = (bytes.load(Ordering::Relaxed), Instant::now());
    std::thread::sleep(Duration::from_secs(secs.min(3)));
    let kbps2 =
        (bytes.load(Ordering::Relaxed) - b1) as f64 * 8.0 / t1.elapsed().as_secs_f64() / 1000.0;
    println!("after set_video_bitrate_kbps({half}): measured {kbps2:.0} kbit/s");
    sender.stop()?;
    Ok(())
}
