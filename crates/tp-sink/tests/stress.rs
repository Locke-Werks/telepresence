use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tp_sink::layout::{AUDIO_CAPACITY_FRAMES, FORMAT_NV12};
use tp_sink::{AudioReader, AudioWriter, VideoRead, VideoReader, VideoWriter};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Local namespace on Windows: Global needs SeCreateGlobalPrivilege, which a
/// test runner should not depend on.
fn unique(kind: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        format!(r"Local\tp-sink-test-{kind}-{pid}-{n}")
    } else {
        format!("/tpst-{kind}-{pid}-{n}")
    }
}

struct Cleanup(String);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = tp_sink::unlink(&self.0);
    }
}

/// Every byte depends on the frame counter and its position, so any mix of
/// two frames, or a partially written one, fails the check.
fn fill_pattern(buf: &mut [u8], counter: u64) {
    let seed = counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (seed.wrapping_add(i as u64 * 31) >> 7) as u8 ^ (i as u8);
    }
}

fn check_pattern(buf: &[u8], counter: u64) -> bool {
    let seed = counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    buf.iter()
        .enumerate()
        .all(|(i, &b)| b == (seed.wrapping_add(i as u64 * 31) >> 7) as u8 ^ (i as u8))
}

#[test]
fn video_no_torn_frames_large() {
    video_stress(320, 240, 640, 480);
}

/// Tiny frames push the frame rate high enough that readers get lapped and the
/// seqlock retry path actually runs.
#[test]
fn video_no_torn_frames_small_fast() {
    video_stress(16, 8, 32, 16);
}

fn video_stress(w0: u32, h0: u32, w1: u32, h1: u32) {
    let name = unique("v");
    let _c = Cleanup(name.clone());
    let mut w = VideoWriter::create(&name, w1, h1).unwrap();
    w.set_format(w0, h0).unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..3 {
        let name = name.clone();
        let done = done.clone();
        readers.push(thread::spawn(move || {
            let mut r = VideoReader::open(&name).unwrap();
            let mut buf = Vec::new();
            let (mut frames, mut last, mut busy) = (0u64, 0u64, 0u64);
            while !done.load(Ordering::Acquire) {
                match r.read_latest(&mut buf) {
                    VideoRead::Frame(info) => {
                        assert_eq!(info.format, FORMAT_NV12);
                        assert_eq!(buf.len(), (info.stride * info.height * 3 / 2) as usize);
                        assert!(check_pattern(&buf, info.frame_counter), "torn frame {}", info.frame_counter);
                        assert!(info.frame_counter >= last, "went backwards");
                        if info.frame_counter != last {
                            frames += 1;
                        }
                        last = info.frame_counter;
                    }
                    VideoRead::Busy => busy += 1,
                    VideoRead::NoSignal => {}
                }
            }
            (frames, busy)
        }));
    }

    // Alternate geometry mid-run to exercise generation changes alongside the
    // seqlock; the pattern covers whatever size the slot claims.
    let start = Instant::now();
    let mut buf = Vec::new();
    let mut n = 0u64;
    while start.elapsed() < Duration::from_secs(2) {
        if n.is_multiple_of(500) {
            let (wd, ht) = if (n / 500).is_multiple_of(2) { (w0, h0) } else { (w1, h1) };
            w.set_format(wd, ht).unwrap();
        }
        buf.resize(w.frame_size(), 0);
        n += 1;
        fill_pattern(&mut buf, n);
        assert_eq!(w.write_frame(&buf, n).unwrap(), n);
    }
    done.store(true, Ordering::Release);
    for r in readers {
        let (frames, busy) = r.join().unwrap();
        eprintln!("video reader: {frames} distinct frames verified, {busy} busy, writer wrote {n}");
        assert!(frames > 100);
    }
}

#[test]
fn video_reader_sees_resize_and_no_signal() {
    let name = unique("vr");
    let _c = Cleanup(name.clone());
    let mut w = VideoWriter::create(&name, 1920, 1080).unwrap();
    let mut r = VideoReader::open(&name).unwrap();
    let mut buf = Vec::new();
    assert_eq!(r.read_latest(&mut buf), VideoRead::NoSignal);

    w.set_format(1280, 720).unwrap();
    let mut f = vec![0u8; w.frame_size()];
    fill_pattern(&mut f, 1);
    w.write_frame(&f, 10).unwrap();
    let VideoRead::Frame(a) = r.read_latest(&mut buf) else { panic!("no frame") };
    assert!(a.format_changed);
    assert_eq!((a.width, a.height, a.timestamp_us), (1280, 720, 10));

    w.write_frame(&f, 11).unwrap();
    let VideoRead::Frame(b) = r.read_latest(&mut buf) else { panic!("no frame") };
    assert!(!b.format_changed);

    w.set_format(1920, 1080).unwrap();
    assert!(w.set_format(3840, 2160).is_err());
    let mut f = vec![0u8; w.frame_size()];
    fill_pattern(&mut f, 3);
    w.write_frame(&f, 12).unwrap();
    let VideoRead::Frame(c) = r.read_latest(&mut buf) else { panic!("no frame") };
    assert!(c.format_changed);
    assert!(c.generation > b.generation);
    assert!(check_pattern(&buf, 3));

    drop(w);
    assert_eq!(r.read_latest(&mut buf), VideoRead::NoSignal);
}

#[test]
fn video_writer_restart_adopts_region() {
    let name = unique("vw");
    let _c = Cleanup(name.clone());
    let mut w = VideoWriter::create(&name, 640, 480).unwrap();
    let mut r = VideoReader::open(&name).unwrap();
    w.set_format(64, 48).unwrap();
    let f = vec![7u8; w.frame_size()];
    w.write_frame(&f, 1).unwrap();
    drop(w);

    let mut w = VideoWriter::create(&name, 640, 480).unwrap();
    w.set_format(64, 48).unwrap();
    w.write_frame(&f, 2).unwrap();
    let mut buf = Vec::new();
    let VideoRead::Frame(info) = r.read_latest(&mut buf) else { panic!("no frame") };
    assert_eq!(info.timestamp_us, 2);
    assert_eq!(info.frame_counter, 2, "counter continues across restart");
}

fn frame_value(idx: u64) -> f32 {
    (idx % (1 << 24)) as f32
}

#[test]
fn audio_ordering_under_contention() {
    let name = unique("a");
    let _c = Cleanup(name.clone());
    let mut w = AudioWriter::create(&name, 2).unwrap();
    let mut r = AudioReader::open(&name, 480).unwrap();
    let done = Arc::new(AtomicBool::new(false));

    let d = done.clone();
    let reader = thread::spawn(move || {
        let mut out = vec![0f32; 2 * 256];
        let mut expect: Option<u64> = None;
        let (mut got, mut skips, mut silence) = (0u64, 0u64, 0u64);
        while !d.load(Ordering::Acquire) {
            let start = r.cursor();
            let s = r.read(&mut out);
            if s.skipped > 0 {
                skips += 1;
                expect = None;
            }
            silence += s.silence as u64;
            for i in 0..s.frames {
                let idx = start + s.skipped + i as u64;
                let (l, rr) = (out[2 * i], out[2 * i + 1]);
                assert_eq!(l, frame_value(idx), "frame {idx} out of order");
                assert_eq!(rr, -frame_value(idx));
                if let Some(e) = expect {
                    assert_eq!(idx, e);
                }
                expect = Some(idx + 1);
            }
            for v in &out[s.frames * 2..] {
                assert_eq!(*v, 0.0);
            }
            got += s.frames as u64;
            thread::yield_now();
        }
        (got, skips, silence)
    });

    let mut idx = 0u64;
    let mut chunk = vec![0f32; 2 * 480];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        for f in 0..480 {
            chunk[2 * f] = frame_value(idx + f as u64);
            chunk[2 * f + 1] = -frame_value(idx + f as u64);
        }
        w.write(&chunk).unwrap();
        idx += 480;
        thread::sleep(Duration::from_micros(500));
    }
    done.store(true, Ordering::Release);
    let (got, skips, silence) = reader.join().unwrap();
    eprintln!("audio: wrote {idx} frames, reader got {got}, {skips} skips, {silence} silence frames");
    assert!(got > idx / 2);
}

#[test]
fn audio_overflow_skips_to_recent() {
    let name = unique("ao");
    let _c = Cleanup(name.clone());
    let mut w = AudioWriter::create(&name, 1).unwrap();
    let mut r = AudioReader::open(&name, 960).unwrap();
    // Two full seconds with nobody reading: four times the ring.
    let total = 96_000u64;
    let data: Vec<f32> = (0..total).map(frame_value).collect();
    for chunk in data.chunks(1000) {
        w.write(chunk).unwrap();
    }
    let mut out = vec![0f32; 480];
    let s = r.read(&mut out);
    assert!(s.skipped > 0);
    assert_eq!(s.frames, 480);
    assert_eq!(r.cursor(), total - 960 + 480);
    let first = total - 960;
    for (i, v) in out.iter().enumerate() {
        assert_eq!(*v, frame_value(first + i as u64));
    }

    // A single write larger than the ring keeps only its tail.
    let big: Vec<f32> = (total..total + AUDIO_CAPACITY_FRAMES as u64 * 2).map(frame_value).collect();
    w.write(&big).unwrap();
    let end = total + AUDIO_CAPACITY_FRAMES as u64 * 2;
    let s = r.read(&mut out);
    assert!(s.skipped > 0);
    assert_eq!(out[0], frame_value(end - 960));
}

#[test]
fn audio_underrun_fills_silence() {
    let name = unique("au");
    let _c = Cleanup(name.clone());
    let mut w = AudioWriter::create(&name, 2).unwrap();
    let mut r = AudioReader::open(&name, 0).unwrap();
    let mut out = vec![1f32; 2 * 100];

    let s = r.read(&mut out);
    assert_eq!((s.frames, s.silence, s.no_signal), (0, 100, false));
    assert!(out.iter().all(|v| *v == 0.0));

    let chunk: Vec<f32> = (0..60).flat_map(|i| [i as f32 + 1.0, -(i as f32) - 1.0]).collect();
    w.write(&chunk).unwrap();
    out.fill(1.0);
    let s = r.read(&mut out);
    assert_eq!((s.frames, s.silence), (60, 40));
    assert_eq!(&out[..120], &chunk[..]);
    assert!(out[120..].iter().all(|v| *v == 0.0));

    // Writer gone: no signal, silence.
    drop(w);
    out.fill(1.0);
    let s = r.read(&mut out);
    assert!(s.no_signal);
    assert!(out.iter().all(|v| *v == 0.0));
}

#[test]
fn audio_stale_heartbeat_is_no_signal() {
    let name = unique("as");
    let _c = Cleanup(name.clone());
    let mut w = AudioWriter::create(&name, 1).unwrap();
    let mut r = AudioReader::open(&name, 0).unwrap();
    w.write(&[0.5; 10]).unwrap();
    thread::sleep(Duration::from_micros(tp_sink::STALE_AFTER_US + 100_000));
    let mut out = [1f32; 10];
    assert!(r.read(&mut out).no_signal);
    w.heartbeat();
    let s = r.read(&mut out);
    assert!(!s.no_signal);
    assert_eq!(s.frames, 10);
}

#[test]
fn reader_open_missing_fails() {
    assert!(VideoReader::open(&unique("missing")).is_err());
    assert!(AudioReader::open(&unique("missing"), 0).is_err());
}
