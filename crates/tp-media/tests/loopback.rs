//! Sender appsink to receiver appsrc in one process, with test sources
//! standing in for the camera and microphone.

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use tp_media::*;

const W: u32 = 640;
const H: u32 = 360;

#[test]
fn end_to_end_video_and_audio() {
    init().unwrap();

    let (ftx, frames) = mpsc::channel::<DecodedFrame>();
    let ftx = Mutex::new(ftx);
    let vrx = VideoReceiver::new(
        VideoReceiverConfig::default(),
        Box::new(move |f| {
            let _ = ftx.lock().unwrap().send(f);
        }),
    )
    .unwrap();
    let (atx, arx) = mpsc::channel::<DecodedAudio>();
    let atx = Mutex::new(atx);
    let arx_ = AudioReceiver::new(
        AudioReceiverConfig::default(),
        Box::new(move |a| {
            let _ = atx.lock().unwrap().send(a);
        }),
    )
    .unwrap();
    vrx.start().unwrap();
    arx_.start().unwrap();
    let vrx = Arc::new(vrx);
    let arcv = Arc::new(arx_);

    let keyframes = Arc::new(Mutex::new(0u32));
    let cfg = SenderConfig {
        video: Some(VideoSource::TestPattern),
        audio: Some(AudioSource::TestTone),
        video_size: Some((W, H)),
        video_fps: Some(30),
        video_bitrate_kbps: 2000,
        ..Default::default()
    };
    let sender = Sender::new(
        cfg,
        {
            let vrx = vrx.clone();
            let keyframes = keyframes.clone();
            Box::new(move |v: EncodedVideo| {
                if v.keyframe {
                    *keyframes.lock().unwrap() += 1;
                }
                let _ = vrx.push(&v.data, v.pts_us, v.keyframe);
            })
        },
        {
            let arcv = arcv.clone();
            Box::new(move |a: EncodedAudio| {
                let _ = arcv.push(&a.data, a.pts_us);
            })
        },
    )
    .unwrap();
    println!("encoder: {:?}", sender.encoder().map(|e| e.factory_name()));
    println!("decoder: {}", vrx.decoder().factory_name());
    sender.start().unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_frames = 0;
    while got_frames < 10 {
        let left = deadline.saturating_duration_since(Instant::now());
        sender.check_error().unwrap();
        vrx.check_error().unwrap();
        let f = frames
            .recv_timeout(left)
            .expect("timed out waiting for decoded frames");
        assert_eq!((f.width, f.height), (W, H));
        assert!(f.stride >= W);
        assert!(f.data.len() >= (f.stride * H * 3 / 2) as usize);
        got_frames += 1;
    }

    // Exercise the runtime controls while frames flow.
    assert!(sender.set_video_bitrate_kbps(1500));
    let before = *keyframes.lock().unwrap();
    sender.request_keyframe();

    let mut audio_samples = 0usize;
    while audio_samples < 48_000 / 2 {
        let left = deadline.saturating_duration_since(Instant::now());
        arcv.check_error().unwrap();
        let a = arx
            .recv_timeout(left)
            .expect("timed out waiting for decoded audio");
        assert_eq!(a.rate, 48_000);
        assert_eq!(a.channels, 1);
        assert!(a.samples.iter().all(|s| s.is_finite()));
        audio_samples += a.samples.len();
    }

    std::thread::sleep(Duration::from_millis(300));
    let after = *keyframes.lock().unwrap();
    println!("keyframes before request: {before}, after: {after}");
    assert!(after > before, "forced keyframe did not arrive");

    sender.stop().unwrap();
    vrx.stop().unwrap();
    arcv.stop().unwrap();
}

#[test]
fn device_enumeration_runs() {
    let devs = list_devices().unwrap();
    for d in &devs {
        println!("{:?} {:?} [{:?}]", d.kind, d.name, d.api);
    }
    println!(
        "vb-cable: {:?}",
        find_vb_cable_input().unwrap().map(|d| d.name)
    );
}
