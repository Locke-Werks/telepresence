//! Picks the capture mode a camera natively produces, so the sender encodes
//! exactly what the sensor delivers instead of scaling to a fixed size.

use gst::prelude::*;

/// Upper bound on frame rate when choosing a mode. Higher rates cost bitrate
/// a call cannot use and are rarely what a webcam is good at.
pub const MAX_FPS: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeFormat {
    /// `video/x-raw` with this format string, e.g. "NV12".
    Raw(String),
    /// `image/jpeg` (MJPEG), needs a decoder before the encoder.
    Jpeg,
}

impl ModeFormat {
    /// Lower is better. NV12 is what hardware encoders eat directly; packed
    /// 4:2:2 is a cheap conversion; MJPEG costs a full decode per frame.
    fn rank(&self) -> u8 {
        match self {
            ModeFormat::Raw(f) => match f.as_str() {
                "NV12" => 0,
                "YUY2" => 1,
                "UYVY" => 2,
                "I420" => 3,
                _ => 4,
            },
            ModeFormat::Jpeg => 5,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            ModeFormat::Raw(f) => f,
            ModeFormat::Jpeg => "MJPEG",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    pub fps_num: i32,
    pub fps_den: i32,
    pub format: ModeFormat,
}

impl CameraMode {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den.max(1) as f64
    }

    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Caps that pin the source to exactly this mode.
    pub fn caps(&self) -> gst::Caps {
        let b = match &self.format {
            ModeFormat::Raw(f) => gst::Caps::builder("video/x-raw").field("format", f.as_str()),
            ModeFormat::Jpeg => gst::Caps::builder("image/jpeg"),
        };
        b.field("width", self.width as i32)
            .field("height", self.height as i32)
            .field("framerate", gst::Fraction::new(self.fps_num, self.fps_den))
            .build()
    }
}

impl std::fmt::Display for CameraMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fps = self.fps();
        if (fps - fps.round()).abs() < 0.01 {
            write!(
                f,
                "{}x{}@{} {}",
                self.width,
                self.height,
                fps.round(),
                self.format.name()
            )
        } else {
            write!(
                f,
                "{}x{}@{:.2} {}",
                self.width,
                self.height,
                fps,
                self.format.name()
            )
        }
    }
}

/// H.264 at about 0.1 bit per pixel per frame looks clean for camera content
/// at conversational motion; 1080p30 lands near 6 Mbit/s.
pub fn default_bitrate_kbps(width: u32, height: u32, fps: f64) -> u32 {
    let bits = width as f64 * height as f64 * fps.max(1.0) * 0.1;
    ((bits / 1000.0).round() as u32).clamp(1_000, 40_000)
}

/// Optional constraints on the choice. Size must match exactly; the frame
/// rate replaces `MAX_FPS` as the ceiling.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModeOverride {
    pub size: Option<(u32, u32)>,
    pub fps: Option<u32>,
}

/// True for landscape 16:9 or 4:3 within about 1%. Cameras also advertise
/// square and odd sensor crops (1552x1552, 1760x1328 on a MacBook Air) and
/// portrait twins of their landscape modes; a call wants a standard frame, so
/// those only win when nothing standard is on offer.
pub fn is_standard_aspect(width: u32, height: u32) -> bool {
    if width < height || height == 0 {
        return false;
    }
    let r = width as f64 / height as f64;
    [16.0 / 9.0, 4.0 / 3.0]
        .iter()
        .any(|t| (r / t - 1.0).abs() <= 0.01)
}

/// Standard aspect first, then largest pixel count, then highest frame rate
/// within the ceiling, then the cheapest format to feed an encoder. Returns
/// None when nothing usable (or nothing matching the override) is advertised.
pub fn choose_mode(caps: &gst::CapsRef, ov: ModeOverride) -> Option<CameraMode> {
    let cap = ov.fps.unwrap_or(MAX_FPS).max(1);
    type Key = (bool, u64, bool, i64, bool, i8);
    let mut best: Option<(CameraMode, Key)> = None;
    for (s, features) in caps.iter_with_features() {
        // GL or other device memory needs an upload/download step and is
        // offered alongside the same mode in system memory anyway.
        if !features.is_any() && !features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY) {
            continue;
        }
        let Some(m) = mode_from_structure(s, cap) else {
            continue;
        };
        if let Some((w, h)) = ov.size
            && (m.width != w || m.height != h)
        {
            continue;
        }
        let fps = m.fps();
        let within = fps <= cap as f64 + 0.01;
        // Over the ceiling, the closest rate above it is the least bad.
        let fps_key = if within {
            (fps * 1000.0) as i64
        } else {
            -(fps * 1000.0) as i64
        };
        // Cameras list portrait twins of their landscape modes (1080x1920
        // beside 1920x1080); without a rotation tag, landscape is the native one.
        let key = (
            is_standard_aspect(m.width, m.height),
            m.pixels(),
            within,
            fps_key,
            m.width >= m.height,
            -(m.format.rank() as i8),
        );
        if best.as_ref().is_none_or(|(_, k)| key > *k) {
            best = Some((m, key));
        }
    }
    best.map(|(m, _)| m)
}

/// Every distinct size and rate the camera offers in system memory, best
/// first by the same order `choose_mode` uses, cheapest format kept per pair.
/// For a mode picker; ranges collapse to their largest size.
pub fn list_modes(caps: &gst::CapsRef) -> Vec<CameraMode> {
    let mut out: Vec<CameraMode> = Vec::new();
    for (s, features) in caps.iter_with_features() {
        if !features.is_any() && !features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY) {
            continue;
        }
        let Some(m) = mode_from_structure(s, MAX_FPS) else {
            continue;
        };
        match out.iter_mut().find(|o| {
            o.width == m.width && o.height == m.height && o.fps_num * m.fps_den == m.fps_num * o.fps_den
        }) {
            Some(o) if m.format.rank() < o.format.rank() => *o = m,
            Some(_) => {}
            None => out.push(m),
        }
    }
    out.sort_by(|a, b| {
        let k = |m: &CameraMode| (is_standard_aspect(m.width, m.height), m.pixels(), (m.fps() * 1000.0) as i64);
        k(b).cmp(&k(a))
    });
    out
}

fn mode_from_structure(s: &gst::StructureRef, cap: u32) -> Option<CameraMode> {
    let format = match s.name().as_str() {
        "video/x-raw" => ModeFormat::Raw(best_format(s.value("format").ok()?)?),
        "image/jpeg" => ModeFormat::Jpeg,
        _ => return None,
    };
    let width = max_int(s.value("width").ok()?)?;
    let height = max_int(s.value("height").ok()?)?;
    let (fps_num, fps_den) = pick_fps(s.value("framerate").ok()?, cap)?;
    if width == 0 || height == 0 || fps_num <= 0 {
        return None;
    }
    Some(CameraMode {
        width,
        height,
        fps_num,
        fps_den,
        format,
    })
}

fn best_format(v: &gst::glib::SendValue) -> Option<String> {
    let all: Vec<String> = if let Ok(f) = v.get::<String>() {
        vec![f]
    } else if let Ok(l) = v.get::<gst::List>() {
        l.iter().filter_map(|x| x.get::<String>().ok()).collect()
    } else {
        return None;
    };
    all.into_iter()
        .min_by_key(|f| ModeFormat::Raw(f.clone()).rank())
}

fn max_int(v: &gst::glib::SendValue) -> Option<u32> {
    if let Ok(i) = v.get::<i32>() {
        return u32::try_from(i).ok();
    }
    if let Ok(r) = v.get::<gst::IntRange<i32>>() {
        return u32::try_from(r.max()).ok();
    }
    if let Ok(l) = v.get::<gst::List>() {
        return l
            .iter()
            .filter_map(|x| x.get::<i32>().ok())
            .max()
            .and_then(|i| u32::try_from(i).ok());
    }
    None
}

fn frac_f(f: gst::Fraction) -> f64 {
    f.numer() as f64 / f.denom().max(1) as f64
}

/// Highest rate at or under the ceiling; if every rate is above it, the lowest.
fn pick_fps(v: &gst::glib::SendValue, cap: u32) -> Option<(i32, i32)> {
    let capf = cap as f64 + 0.01;
    let mut opts: Vec<gst::Fraction> = Vec::new();
    if let Ok(f) = v.get::<gst::Fraction>() {
        opts.push(f);
    } else if let Ok(r) = v.get::<gst::FractionRange>() {
        let (lo, hi) = (r.min(), r.max());
        if frac_f(hi) <= capf {
            opts.push(hi);
        } else if frac_f(lo) <= capf {
            opts.push(gst::Fraction::new(cap as i32, 1));
        } else {
            opts.push(lo);
        }
    } else if let Ok(l) = v.get::<gst::List>() {
        opts.extend(l.iter().filter_map(|x| x.get::<gst::Fraction>().ok()));
    } else {
        return None;
    }
    let under = opts
        .iter()
        .filter(|f| frac_f(**f) <= capf && f.numer() > 0)
        .max_by(|a, b| frac_f(**a).total_cmp(&frac_f(**b)));
    let pick = under.or_else(|| {
        opts.iter()
            .min_by(|a, b| frac_f(**a).total_cmp(&frac_f(**b)))
    })?;
    Some((pick.numer(), pick.denom()))
}

/// Caps the camera advertises: the device monitor's copy when present,
/// otherwise the source pad's caps once the element is in READY.
pub fn device_caps(device: Option<&gst::Device>, source: &gst::Element) -> Option<gst::Caps> {
    if let Some(c) = device.and_then(|d| d.caps())
        && !c.is_empty()
        && !c.is_any()
    {
        return Some(c);
    }
    let prev = source.current_state();
    if source.set_state(gst::State::Ready).is_err() {
        let _ = source.set_state(prev);
        return None;
    }
    let caps = source.static_pad("src").map(|p| p.query_caps(None));
    let _ = source.set_state(prev);
    caps.filter(|c| !c.is_empty() && !c.is_any())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(s: &str) -> gst::Caps {
        gst::init().unwrap();
        s.parse().unwrap()
    }

    #[test]
    fn picks_largest_then_fastest() {
        let c = caps(
            "video/x-raw,format=NV12,width=1280,height=720,framerate=60/1; \
             video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1; \
             video/x-raw,format=NV12,width=1920,height=1080,framerate=15/1",
        );
        let m = choose_mode(&c, ModeOverride::default()).unwrap();
        assert_eq!(
            (m.width, m.height, m.fps_num, m.fps_den),
            (1920, 1080, 30, 1)
        );
    }

    #[test]
    fn prefers_raw_over_mjpeg_at_equal_mode() {
        let c = caps(
            "image/jpeg,width=1920,height=1080,framerate=30/1; \
             video/x-raw,format=YUY2,width=1920,height=1080,framerate=30/1",
        );
        let m = choose_mode(&c, ModeOverride::default()).unwrap();
        assert_eq!(m.format, ModeFormat::Raw("YUY2".into()));
    }

    #[test]
    fn mjpeg_wins_when_it_is_the_only_way_to_the_resolution() {
        let c = caps(
            "image/jpeg,width=1920,height=1080,framerate=30/1; \
             video/x-raw,format=YUY2,width=1280,height=720,framerate=30/1",
        );
        let m = choose_mode(&c, ModeOverride::default()).unwrap();
        assert_eq!(m.format, ModeFormat::Jpeg);
        assert_eq!(m.width, 1920);
    }

    #[test]
    fn caps_rate_at_sixty_and_handles_ranges_and_lists() {
        let c = caps(
            "video/x-raw,format={ UYVY, NV12 },width=[ 160, 1920 ],height=[ 120, 1080 ],\
             framerate=[ 1/1, 120/1 ]",
        );
        let m = choose_mode(&c, ModeOverride::default()).unwrap();
        assert_eq!((m.width, m.height, m.fps_num), (1920, 1080, 60));
        assert_eq!(m.format, ModeFormat::Raw("NV12".into()));

        let c =
            caps("video/x-raw,format=NV12,width=1280,height=720,framerate={ 120/1, 60/1, 30/1 }");
        assert_eq!(
            choose_mode(&c, ModeOverride::default()).unwrap().fps_num,
            60
        );
    }

    #[test]
    fn portrait_mode_is_kept() {
        let c = caps("video/x-raw,format=NV12,width=1080,height=1920,framerate=30/1");
        let m = choose_mode(&c, ModeOverride::default()).unwrap();
        assert_eq!((m.width, m.height), (1080, 1920));
    }

    /// The MacBook Air camera's list. 1760x1328 is 1.325:1, inside 1% of 4:3,
    /// and has more pixels than 1080p, so "largest standard mode" takes it.
    #[test]
    fn macbook_air_list_takes_largest_standard_mode() {
        let c = caps(
            "video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1; \
             video/x-raw,format=NV12,width=1080,height=1920,framerate=30/1; \
             video/x-raw,format=NV12,width=1552,height=1552,framerate=30/1; \
             video/x-raw,format=NV12,width=1760,height=1328,framerate=30/1; \
             video/x-raw,format=NV12,width=1328,height=1760,framerate=30/1; \
             video/x-raw,format=NV12,width=1280,height=720,framerate=30/1; \
             video/x-raw,format=NV12,width=640,height=480,framerate=30/1",
        );
        let m = choose_mode(&c, ModeOverride::default()).unwrap();
        assert_eq!((m.width, m.height, m.fps_num), (1760, 1328, 30));
    }

    #[test]
    fn standard_aspect_beats_larger_square_and_portrait() {
        let c = caps(
            "video/x-raw,format=NV12,width=1552,height=1552,framerate=30/1; \
             video/x-raw,format=NV12,width=1080,height=1920,framerate=30/1; \
             video/x-raw,format=NV12,width=1280,height=720,framerate=30/1",
        );
        let m = choose_mode(&c, ModeOverride::default()).unwrap();
        assert_eq!((m.width, m.height), (1280, 720));
    }

    #[test]
    fn square_wins_only_without_a_standard_mode() {
        let c = caps(
            "video/x-raw,format=NV12,width=1552,height=1552,framerate=30/1; \
             video/x-raw,format=NV12,width=1080,height=1920,framerate=30/1",
        );
        let m = choose_mode(&c, ModeOverride::default()).unwrap();
        // Neither is standard, so pixels decide: the 2.41 MP square beats the
        // 2.07 MP portrait.
        assert_eq!((m.width, m.height), (1552, 1552));
    }

    #[test]
    fn list_modes_dedupes_and_orders() {
        let c = caps(
            "image/jpeg,width=1280,height=720,framerate=30/1; \
             video/x-raw,format=YUY2,width=1280,height=720,framerate=30/1; \
             video/x-raw,format=NV12,width=1552,height=1552,framerate=30/1; \
             video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1",
        );
        let l = list_modes(&c);
        let dims: Vec<_> = l.iter().map(|m| (m.width, m.height)).collect();
        assert_eq!(dims, vec![(1920, 1080), (1280, 720), (1552, 1552)]);
        assert_eq!(l[1].format, ModeFormat::Raw("YUY2".into()));
    }

    #[test]
    fn standard_aspect_tolerance() {
        assert!(is_standard_aspect(1920, 1080));
        assert!(is_standard_aspect(640, 480));
        assert!(is_standard_aspect(1760, 1328));
        assert!(is_standard_aspect(854, 480));
        assert!(!is_standard_aspect(1080, 1920));
        assert!(!is_standard_aspect(1552, 1552));
        assert!(!is_standard_aspect(2560, 1080));
        assert!(!is_standard_aspect(1280, 1024));
    }

    #[test]
    fn skips_gl_memory() {
        let c = caps(
            "video/x-raw(memory:GLMemory),format=RGBA,width=3840,height=2160,framerate=30/1; \
             video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1",
        );
        assert_eq!(
            choose_mode(&c, ModeOverride::default()).unwrap().width,
            1920
        );
    }

    #[test]
    fn override_filters_size_and_caps_rate() {
        let c = caps(
            "video/x-raw,format=NV12,width=1920,height=1080,framerate={ 60/1, 30/1 }; \
             video/x-raw,format=NV12,width=1280,height=720,framerate=60/1",
        );
        let m = choose_mode(
            &c,
            ModeOverride {
                size: Some((1920, 1080)),
                fps: Some(30),
            },
        )
        .unwrap();
        assert_eq!((m.width, m.fps_num), (1920, 30));
        assert!(
            choose_mode(
                &c,
                ModeOverride {
                    size: Some((640, 480)),
                    fps: None
                }
            )
            .is_none()
        );
    }

    #[test]
    fn bitrate_scales_and_clamps() {
        assert_eq!(default_bitrate_kbps(1920, 1080, 30.0), 6221);
        assert_eq!(default_bitrate_kbps(1280, 720, 30.0), 2765);
        assert_eq!(default_bitrate_kbps(160, 120, 5.0), 1_000);
        assert_eq!(default_bitrate_kbps(7680, 4320, 60.0), 40_000);
    }
}
