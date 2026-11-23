use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gst_app::AppSink;

use crate::codec::{H264Encoder, select_encoder};
use crate::devices::{DeviceInfo, DeviceKind, find_device, list_devices};
use crate::mode::{self, CameraMode, ModeFormat, ModeOverride, choose_mode, default_bitrate_kbps};
use crate::util::{clock_time_to_us, make, take_error, try_set};

#[derive(Debug, Clone)]
pub struct EncodedVideo {
    /// One H.264 access unit, Annex B byte-stream, SPS/PPS repeated on IDR.
    pub data: Vec<u8>,
    /// Pipeline running time; shares a clock with `EncodedAudio::pts_us`.
    pub pts_us: u64,
    pub keyframe: bool,
}

#[derive(Debug, Clone)]
pub struct EncodedAudio {
    /// One Opus packet (10 ms).
    pub data: Vec<u8>,
    pub pts_us: u64,
}

#[derive(Debug, Clone)]
pub enum VideoSource {
    /// Platform default camera.
    Default,
    /// Camera whose display name contains this string.
    Named(String),
    /// videotestsrc, for tests and diagnostics.
    TestPattern,
}

#[derive(Debug, Clone)]
pub enum AudioSource {
    Default,
    Named(String),
    /// audiotestsrc sine.
    TestTone,
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub video: Option<VideoSource>,
    pub audio: Option<AudioSource>,
    #[deprecated(note = "ignored: the camera's native size is used; set video_size to override")]
    pub width: u32,
    #[deprecated(note = "ignored: the camera's native size is used; set video_size to override")]
    pub height: u32,
    #[deprecated(note = "ignored: the camera's native rate is used; set video_fps to override")]
    pub fps: u32,
    /// Force this capture size. None takes the camera's largest native mode;
    /// for the test pattern None means 1280x720.
    pub video_size: Option<(u32, u32)>,
    /// Ceiling on capture frame rate. None means up to 60 for a camera and
    /// exactly 30 for the test pattern.
    pub video_fps: Option<u32>,
    /// Initial target in kbit/s. 0 derives it from the capture mode, about
    /// 0.1 bit per pixel per frame, clamped to 1..40 Mbit/s.
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_bps: u32,
    pub audio_channels: u32,
    /// Force a specific encoder instead of the preference order.
    pub encoder: Option<H264Encoder>,
}

impl Default for SenderConfig {
    #[allow(deprecated)]
    fn default() -> Self {
        Self {
            video: Some(VideoSource::Default),
            audio: Some(AudioSource::Default),
            width: 0,
            height: 0,
            fps: 0,
            video_size: None,
            video_fps: None,
            video_bitrate_kbps: 0,
            audio_bitrate_bps: 64_000,
            audio_channels: 1,
            encoder: None,
        }
    }
}

/// What the video branch actually encodes, known once caps negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    /// After orientation correction, so a rotated camera reports upright size.
    pub width: u32,
    pub height: u32,
    pub fps_num: i32,
    pub fps_den: i32,
}

pub type VideoCallback = Box<dyn Fn(EncodedVideo) + Send + Sync + 'static>;
pub type AudioCallback = Box<dyn Fn(EncodedAudio) + Send + Sync + 'static>;

/// Video and audio live in one pipeline so both branches run on the same
/// clock and base time, which makes their running-time PTS directly comparable.
pub struct Sender {
    pipeline: gst::Pipeline,
    encoder: Option<(H264Encoder, gst::Element)>,
    capture_mode: Option<CameraMode>,
    initial_bitrate_kbps: u32,
    video_format: Arc<Mutex<Option<VideoFormat>>>,
}

impl Sender {
    pub fn new(
        cfg: SenderConfig,
        on_video: VideoCallback,
        on_audio: AudioCallback,
    ) -> crate::Result<Self> {
        crate::init()?;
        let pipeline = gst::Pipeline::with_name("tp-sender");
        let mut encoder = None;
        let mut capture_mode = None;
        let mut initial_bitrate_kbps = 0;
        let video_format = Arc::new(Mutex::new(None));

        if let Some(src) = &cfg.video {
            let b = build_video_branch(&pipeline, &cfg, src, on_video, video_format.clone())?;
            encoder = Some((b.kind, b.enc));
            capture_mode = b.mode;
            initial_bitrate_kbps = b.kbps;
        }
        if let Some(src) = &cfg.audio {
            build_audio_branch(&pipeline, &cfg, src, on_audio)?;
        }
        Ok(Self {
            pipeline,
            encoder,
            capture_mode,
            initial_bitrate_kbps,
            video_format,
        })
    }

    pub fn start(&self) -> crate::Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub fn stop(&self) -> crate::Result<()> {
        self.pipeline.set_state(gst::State::Null)?;
        Ok(())
    }

    pub fn encoder(&self) -> Option<H264Encoder> {
        self.encoder.as_ref().map(|(k, _)| *k)
    }

    /// The mode the source was pinned to, before orientation correction.
    /// None when the camera's caps could not be read and it negotiated freely.
    pub fn capture_mode(&self) -> Option<&CameraMode> {
        self.capture_mode.as_ref()
    }

    /// The target the encoder started at: the configured value, or the one
    /// derived from the capture mode when the config left it at 0.
    pub fn initial_video_bitrate_kbps(&self) -> u32 {
        self.initial_bitrate_kbps
    }

    /// Encoded size and rate. None until the video branch has negotiated,
    /// which happens shortly after `start`.
    pub fn video_format(&self) -> Option<VideoFormat> {
        *self.video_format.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asks the encoder for an IDR with SPS/PPS, for a receiver that joined late
    /// or lost packets.
    pub fn request_keyframe(&self) {
        let Some((_, enc)) = &self.encoder else {
            return;
        };
        let ev = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if let Some(pad) = enc.static_pad("src") {
            pad.send_event(ev);
        }
    }

    /// All supported encoders take kbit/s and accept the change while playing.
    pub fn set_video_bitrate_kbps(&self, kbps: u32) -> bool {
        match &self.encoder {
            Some((kind, enc)) => kind.set_bitrate(enc, kbps),
            None => false,
        }
    }

    /// Returns the first error posted on the bus, if any, without blocking.
    pub fn check_error(&self) -> crate::Result<()> {
        take_error(&self.pipeline)
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn device_element(kind: DeviceKind, query: &str) -> crate::Result<gst::Element> {
    let dev = find_device(kind, query)?;
    tracing::info!(
        "{kind:?}: {} via {} id {}",
        dev.name,
        dev.api.as_deref().unwrap_or("?"),
        dev.id.as_deref().unwrap_or("?")
    );
    dev.create_element()
}

/// The named camera, or the first one the monitor lists. A device handle,
/// rather than autovideosrc, is what makes its caps readable up front.
pub(crate) fn find_camera(name: Option<&str>) -> crate::Result<Option<DeviceInfo>> {
    match name {
        Some(n) => find_device(DeviceKind::Camera, n).map(Some),
        None => Ok(list_devices()?
            .into_iter()
            .find(|d| d.kind == DeviceKind::Camera)),
    }
}

fn capsfilter(caps: &gst::Caps) -> crate::Result<gst::Element> {
    Ok(gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()?)
}

/// The source plus whatever pins its output, in link order, and the mode
/// used to size the bitrate.
fn video_capture(
    cfg: &SenderConfig,
    src: &VideoSource,
) -> crate::Result<(Vec<gst::Element>, Option<CameraMode>)> {
    if let VideoSource::TestPattern = src {
        let s = make("videotestsrc")?;
        s.set_property("is-live", true);
        let (w, h) = cfg.video_size.unwrap_or((1280, 720));
        let mode = CameraMode {
            width: w,
            height: h,
            fps_num: cfg.video_fps.unwrap_or(30) as i32,
            fps_den: 1,
            format: ModeFormat::Raw("NV12".into()),
        };
        let f = capsfilter(&mode.caps())?;
        return Ok((vec![s, f], Some(mode)));
    }

    let dev = match src {
        VideoSource::Named(n) => find_camera(Some(n))?,
        _ => find_camera(None)?,
    };
    let source = match &dev {
        Some(d) => d.create_element()?,
        None => make("autovideosrc")?,
    };
    let caps = mode::device_caps(dev.as_ref().map(|d| &d.device), &source);
    let ov = ModeOverride {
        size: cfg.video_size,
        fps: cfg.video_fps,
    };
    let chosen = caps.as_ref().and_then(|c| choose_mode(c, ov));
    tracing::debug!(
        "camera {:?} advertises {:?}",
        dev.as_ref().map(|d| d.name.as_str()),
        caps.as_ref().map(|c| c.to_string())
    );

    let mut chain = vec![source];
    match &chosen {
        Some(m) => {
            chain.push(capsfilter(&m.caps())?);
            if m.format == ModeFormat::Jpeg {
                chain.push(make("jpegdec")?);
            }
        }
        None if cfg.video_size.is_some() || cfg.video_fps.is_some() => {
            // No native mode matches an explicit override, so honor it the
            // expensive way. Only reachable when the caller asked for it.
            tracing::warn!("camera has no mode matching the size/fps override; scaling");
            let rate = make("videorate")?;
            rate.set_property("drop-only", true);
            let mut b = gst::Caps::builder("video/x-raw");
            if let Some((w, h)) = cfg.video_size {
                b = b.field("width", w as i32).field("height", h as i32);
            }
            if let Some(f) = cfg.video_fps {
                b = b.field("framerate", gst::Fraction::new(f as i32, 1));
            }
            chain.push(make("videoconvert")?);
            chain.push(make("videoscale")?);
            chain.push(rate);
            chain.push(capsfilter(&b.build())?);
        }
        None => {
            tracing::warn!("could not read camera caps; letting the source negotiate freely");
        }
    }
    Ok((chain, chosen))
}

struct VideoBranch {
    kind: H264Encoder,
    enc: gst::Element,
    mode: Option<CameraMode>,
    kbps: u32,
}

fn build_video_branch(
    pipeline: &gst::Pipeline,
    cfg: &SenderConfig,
    src: &VideoSource,
    on_video: VideoCallback,
    format_out: Arc<Mutex<Option<VideoFormat>>>,
) -> crate::Result<VideoBranch> {
    let (capture, mode) = video_capture(cfg, src)?;
    let kind = cfg
        .encoder
        .or_else(select_encoder)
        .ok_or(crate::Error::NoElement("H.264 encoder"))?;
    let fps = mode.as_ref().map(CameraMode::fps).unwrap_or(30.0);
    let kbps = match (cfg.video_bitrate_kbps, &mode) {
        (0, Some(m)) => default_bitrate_kbps(m.width, m.height, m.fps()),
        (0, None) => default_bitrate_kbps(1280, 720, 30.0),
        (k, _) => k,
    };
    let enc = kind.build(kbps, fps.round().max(1.0) as u32)?;

    // videoflip handles only some formats, so convert on both sides; each
    // videoconvert passes through untouched when the format already fits.
    let convert_in = make("videoconvert")?;
    let flip = make("videoflip")?;
    // Follows image-orientation tags, so a rotated camera arrives upright and
    // the encoded size takes the rotated dimensions.
    try_set(&flip, "video-direction", "auto");
    let convert = make("videoconvert")?;
    let raw_filter = capsfilter(
        &gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .build(),
    )?;
    let queue = leaky_queue()?;
    let parse = make("h264parse")?;
    parse.set_property("config-interval", -1i32);
    let h264_filter = capsfilter(
        &gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
    )?;

    if let Some(pad) = enc.static_pad("sink") {
        let mode_desc = mode
            .as_ref()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "unknown".into());
        let enc_name = kind.factory_name();
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(ev)) = &info.data
                && let gst::EventView::Caps(c) = ev.view()
                && let Ok(vi) = gst_video::VideoInfo::from_caps(c.caps())
            {
                let f = VideoFormat {
                    width: vi.width(),
                    height: vi.height(),
                    fps_num: vi.fps().numer(),
                    fps_den: vi.fps().denom(),
                };
                let mut slot = format_out.lock().unwrap_or_else(|e| e.into_inner());
                if *slot != Some(f) {
                    *slot = Some(f);
                    tracing::info!(
                        "camera native mode: {mode_desc}, encoding {}x{} at {kbps} kbit/s ({enc_name})",
                        f.width,
                        f.height
                    );
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    let sink = AppSink::builder()
        .sync(false)
        .max_buffers(4)
        .drop(false)
        .build();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                on_video(EncodedVideo {
                    data: map.as_slice().to_vec(),
                    pts_us: clock_time_to_us(buf.pts()),
                    keyframe: !buf.flags().contains(gst::BufferFlags::DELTA_UNIT),
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let mut chain: Vec<&gst::Element> = capture.iter().collect();
    chain.extend([
        &convert_in,
        &flip,
        &convert,
        &raw_filter,
        &queue,
        &enc,
        &parse,
        &h264_filter,
        sink.upcast_ref(),
    ]);
    pipeline.add_many(chain.iter().copied())?;
    gst::Element::link_many(chain.iter().copied())?;
    Ok(VideoBranch {
        kind,
        enc,
        mode,
        kbps,
    })
}

fn build_audio_branch(
    pipeline: &gst::Pipeline,
    cfg: &SenderConfig,
    src: &AudioSource,
    on_audio: AudioCallback,
) -> crate::Result<()> {
    let source = match src {
        AudioSource::Default => make("autoaudiosrc")?,
        AudioSource::Named(n) => device_element(DeviceKind::Microphone, n)?,
        AudioSource::TestTone => {
            let s = make("audiotestsrc")?;
            s.set_property("is-live", true);
            // 10 ms buffers so the test source does not add its own latency.
            s.set_property("samplesperbuffer", 480i32);
            s
        }
    };
    try_set(&source, "low-latency", "true");

    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    let caps = gst::Caps::builder("audio/x-raw")
        .field("rate", 48_000i32)
        .field("channels", cfg.audio_channels.clamp(1, 2) as i32)
        .build();
    let filter = capsfilter(&caps)?;
    let queue = leaky_queue()?;
    let enc = make("opusenc")?;
    try_set(&enc, "frame-size", "10");
    try_set(&enc, "audio-type", "restricted-lowdelay");
    try_set(&enc, "bitrate-type", "cbr");
    try_set(&enc, "bitrate", &cfg.audio_bitrate_bps.to_string());

    let sink = AppSink::builder().sync(false).max_buffers(16).build();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                on_audio(EncodedAudio {
                    data: map.as_slice().to_vec(),
                    pts_us: clock_time_to_us(buf.pts()),
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let chain = [
        &source,
        &convert,
        &resample,
        &filter,
        &queue,
        &enc,
        sink.upcast_ref(),
    ];
    pipeline.add_many(chain)?;
    gst::Element::link_many(chain)?;
    Ok(())
}

/// A short queue that discards the oldest data, so a stalled encoder or
/// consumer never builds up latency behind it.
pub(crate) fn leaky_queue() -> crate::Result<gst::Element> {
    Ok(gst::ElementFactory::make("queue")
        .property("max-size-buffers", 2u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .property_from_str("leaky", "downstream")
        .build()?)
}
