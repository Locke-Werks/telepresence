//! Feeds the video sink with SMPTE-style colour bars and a white box that
//! sweeps left to right, so a virtual camera can be checked without the
//! Receiver.
//!
//!     cargo run -p tp-sink --example write_pattern -- [WIDTHxHEIGHT] [SECONDS] [NAME] [FPS]
//!
//! Any even size up to the section's capacity works, portrait included. FPS
//! is an integer or a ratio such as 30000/1001. Defaults: 1280x720, 30
//! seconds, the platform default video name, 30 fps.

use std::time::{Duration, Instant};

use tp_sink::{DEFAULT_VIDEO_NAME, VIDEO_MAX_HEIGHT, VIDEO_MAX_WIDTH, VideoWriter, now_us};

/// BT.601 limited range, which is what MF assumes for NV12 by default.
fn yuv(r: f32, g: f32, b: f32) -> (u8, u8, u8) {
    let y = 16.0 + 65.481 * r + 128.553 * g + 24.966 * b;
    let u = 128.0 - 37.797 * r - 74.203 * g + 112.0 * b;
    let v = 128.0 + 112.0 * r - 93.786 * g - 18.214 * b;
    (y.round() as u8, u.round() as u8, v.round() as u8)
}

const BARS: [(f32, f32, f32); 7] = [
    (0.75, 0.75, 0.75),
    (0.75, 0.75, 0.0),
    (0.0, 0.75, 0.75),
    (0.0, 0.75, 0.0),
    (0.75, 0.0, 0.75),
    (0.75, 0.0, 0.0),
    (0.0, 0.0, 0.75),
];

fn draw(buf: &mut [u8], w: usize, h: usize, frame: u64) {
    let (y_plane, uv_plane) = buf.split_at_mut(w * h);
    let box_w = w / 8;
    let box_x = (frame as usize * 8) % (w - box_w);
    let (box_y0, box_y1) = (h * 3 / 4, h * 3 / 4 + h / 8);
    for row in 0..h {
        for col in 0..w {
            let in_box = col >= box_x && col < box_x + box_w && row >= box_y0 && row < box_y1;
            let (r, g, b) = if in_box { (1.0, 1.0, 1.0) } else { BARS[col * 7 / w] };
            let (yy, u, v) = yuv(r, g, b);
            y_plane[row * w + col] = yy;
            if row % 2 == 0 && col % 2 == 0 {
                let o = (row / 2) * w + col;
                uv_plane[o] = u;
                uv_plane[o + 1] = v;
            }
        }
    }
}

fn parse_fps(s: &str) -> Option<(u32, u32)> {
    match s.split_once('/') {
        Some((n, d)) => Some((n.parse().ok()?, d.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (w, h) = args
        .first()
        .and_then(|s| s.split_once('x'))
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .unwrap_or((1280, 720));
    let seconds: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let name = args.get(2).map(String::as_str).unwrap_or(DEFAULT_VIDEO_NAME);
    let (fps_n, fps_d) = args.get(3).and_then(|s| parse_fps(s)).filter(|&(n, d)| n > 0 && d > 0).unwrap_or((30, 1));

    // The Windows camera creates the section when a client first opens it. A
    // standard user cannot create it, so give the camera a few seconds to
    // appear before open_or_create falls back to a Local name. Asking for the
    // camera's full ceiling first, then only what this frame needs, lets the
    // pattern attach to a smaller section an older camera build created.
    let (mw, mh) = (w.max(VIDEO_MAX_WIDTH), h.max(VIDEO_MAX_HEIGHT));
    let wait_until = Instant::now() + Duration::from_secs(10);
    let mut writer = loop {
        match VideoWriter::open_existing(name, mw, mh).or_else(|_| VideoWriter::open_existing(name, w, h)) {
            Ok(wr) => {
                println!("attached to existing section {name}");
                break wr;
            }
            Err(_) if Instant::now() < wait_until => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => {
                break VideoWriter::open_or_create(name, mw, mh).unwrap_or_else(|e| panic!("open {name}: {e}"));
            }
        }
    };
    writer.set_format(w, h).expect("set_format");
    writer.set_frame_rate(fps_n, fps_d).expect("set_frame_rate");
    let mut frame = vec![0u8; writer.frame_size()];
    println!("writing {w}x{h} at {fps_n}/{fps_d} fps to {name} for {seconds}s");

    let start = Instant::now();
    let period = Duration::from_secs_f64(fps_d as f64 / fps_n as f64);
    let mut n = 0u64;
    while start.elapsed() < Duration::from_secs(seconds) {
        draw(&mut frame, w as usize, h as usize, n);
        writer.write_frame(&frame, now_us()).expect("write_frame");
        n += 1;
        if let Some(wait) = period.mul_f64(n as f64).checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
    }
    println!("wrote {n} frames");
}
