use gst::prelude::*;
use gst_app::{AppSink, AppSrc};
use gst_video::prelude::*;

use crate::codec::{H264Decoder, select_decoder};
use crate::devices::DeviceInfo;
use crate::util::{clock_time_to_us, make, take_error, try_set};

/// A decoded NV12 frame. `data` holds the Y plane followed by the interleaved
/// UV plane, each row `stride` bytes wide.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// Byte offset of the UV plane within `data`.
    pub uv_offset: usize,
    pub data: Vec<u8>,
    pub pts_us: u64,
}

/// Interleaved F32LE at 48 kHz.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    pub channels: u32,
    pub rate: u32,
    pub samples: Vec<f32>,
    pub pts_us: u64,
}

#[derive(Debug, Clone, Default)]
pub struct VideoReceiverConfig {
    /// Also render to an on-screen window.
    pub preview: bool,
    pub decoder: Option<H264Decoder>,
}

pub type FrameCallback = Box<dyn Fn(DecodedFrame) + Send + Sync + 'static>;
pub type AudioCallback = Box<dyn Fn(DecodedAudio) + Send + Sync + 'static>;

pub struct VideoReceiver {
    pipeline: gst::Pipeline,
    src: AppSrc,
    decoder: H264Decoder,
}

impl VideoReceiver {
    pub fn new(cfg: VideoReceiverConfig, on_frame: FrameCallback) -> crate::Result<Self> {
        crate::init()?;
        let decoder = cfg
            .decoder
            .or_else(select_decoder)
            .ok_or(crate::Error::NoElement("H.264 decoder"))?;
        let pipeline = gst::Pipeline::with_name("tp-video-receiver");

        let caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let src = live_appsrc(&caps);
        let parse = make("h264parse")?;
        let dec = make(decoder.factory_name())?;
        let convert = make("videoconvert")?;
        let nv12 = gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .build();
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", &nv12)
            .build()?;
        let tee = make("tee")?;
        let queue = crate::sender::leaky_queue()?;

        let sink = AppSink::builder()
            .caps(&nv12)
            .sync(false)
            .max_buffers(2)
            .drop(true)
            .build();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                    let info = gst_video::VideoInfo::from_caps(caps)
                        .map_err(|_| gst::FlowError::NotNegotiated)?;
                    let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let pts = buf.pts();
                    // VideoFrame honours any VideoMeta, so strides and plane offsets
                    // are right even when the decoder pads rows.
                    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf, &info)
                        .map_err(|_| gst::FlowError::Error)?;
                    let stride = frame.plane_stride()[0] as u32;
                    let uv_stride = frame.plane_stride()[1] as u32;
                    let h = info.height();
                    let w = info.width();
                    let y = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                    let uv = frame.plane_data(1).map_err(|_| gst::FlowError::Error)?;
                    let y_len = (stride * h) as usize;
                    let uv_len = (uv_stride * h.div_ceil(2)) as usize;
                    let mut data = Vec::with_capacity(y_len + uv_len);
                    data.extend_from_slice(&y[..y_len.min(y.len())]);
                    let uv_offset = data.len();
                    data.extend_from_slice(&uv[..uv_len.min(uv.len())]);
                    on_frame(DecodedFrame {
                        width: w,
                        height: h,
                        stride,
                        uv_offset,
                        data,
                        pts_us: clock_time_to_us(pts),
                    });
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let main = [src.upcast_ref(), &parse, &dec, &convert, &filter, &tee];
        pipeline.add_many(main)?;
        gst::Element::link_many(main)?;
        pipeline.add_many([&queue, sink.upcast_ref()])?;
        gst::Element::link_many([&tee, &queue, sink.upcast_ref()])?;

        if cfg.preview {
            let pq = crate::sender::leaky_queue()?;
            let pconv = make("videoconvert")?;
            let psink = make("autovideosink")?;
            psink.set_property("sync", false);
            pipeline.add_many([&pq, &pconv, &psink])?;
            gst::Element::link_many([&tee, &pq, &pconv, &psink])?;
        }

        Ok(Self {
            pipeline,
            src,
            decoder,
        })
    }

    pub fn decoder(&self) -> H264Decoder {
        self.decoder
    }

    pub fn start(&self) -> crate::Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub fn stop(&self) -> crate::Result<()> {
        self.pipeline.set_state(gst::State::Null)?;
        Ok(())
    }

    /// Feeds one access unit as produced by `Sender`.
    pub fn push(&self, data: &[u8], pts_us: u64, keyframe: bool) -> crate::Result<()> {
        let mut buf = gst::Buffer::from_slice(data.to_vec());
        {
            let b = buf.get_mut().expect("fresh buffer is writable");
            b.set_pts(gst::ClockTime::from_useconds(pts_us));
            if !keyframe {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        self.src.push_buffer(buf)?;
        Ok(())
    }

    pub fn check_error(&self) -> crate::Result<()> {
        take_error(&self.pipeline)
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[derive(Debug, Clone)]
pub enum AudioOutput {
    /// A specific output device, e.g. VB-Cable's "CABLE Input".
    Device(DeviceInfo),
    /// Platform default output.
    Default,
}

#[derive(Debug, Clone)]
pub struct AudioReceiverConfig {
    pub channels: u32,
    /// Play straight into a device in addition to the callback.
    pub output: Option<AudioOutput>,
}

impl Default for AudioReceiverConfig {
    fn default() -> Self {
        Self {
            channels: 1,
            output: None,
        }
    }
}

pub struct AudioReceiver {
    pipeline: gst::Pipeline,
    src: AppSrc,
}

impl AudioReceiver {
    pub fn new(cfg: AudioReceiverConfig, on_audio: AudioCallback) -> crate::Result<Self> {
        crate::init()?;
        let pipeline = gst::Pipeline::with_name("tp-audio-receiver");
        let channels = cfg.channels.clamp(1, 2) as i32;
        // Mapping family 0 lets opusdec start without the Ogg-style header
        // packets, which never cross the wire.
        let caps = gst::Caps::builder("audio/x-opus")
            .field("rate", 48_000i32)
            .field("channels", channels)
            .field("channel-mapping-family", 0i32)
            .build();
        let src = live_appsrc(&caps);
        let dec = make("opusdec")?;
        try_set(&dec, "plc", "true");
        let convert = make("audioconvert")?;
        let resample = make("audioresample")?;
        let f32caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("rate", 48_000i32)
            .field("layout", "interleaved")
            .build();
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", &f32caps)
            .build()?;
        let tee = make("tee")?;
        let queue = make("queue")?;

        let sink = AppSink::builder()
            .caps(&f32caps)
            .sync(false)
            .max_buffers(32)
            .build();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                    let info = gst_audio::AudioInfo::from_caps(caps)
                        .map_err(|_| gst::FlowError::NotNegotiated)?;
                    let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let samples = map
                        .as_slice()
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    on_audio(DecodedAudio {
                        channels: info.channels(),
                        rate: info.rate(),
                        samples,
                        pts_us: clock_time_to_us(buf.pts()),
                    });
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let main = [src.upcast_ref(), &dec, &convert, &resample, &filter, &tee];
        pipeline.add_many(main)?;
        gst::Element::link_many(main)?;
        pipeline.add_many([&queue, sink.upcast_ref()])?;
        gst::Element::link_many([&tee, &queue, sink.upcast_ref()])?;

        if let Some(out) = &cfg.output {
            let oq = make("queue")?;
            let oconv = make("audioconvert")?;
            let ores = make("audioresample")?;
            let osink = match out {
                AudioOutput::Device(d) => d.create_element()?,
                AudioOutput::Default => make("autoaudiosink")?,
            };
            // wasapi2sink's low-latency mode drops the device period to the
            // minimum; sync=false because sender timestamps are not our clock.
            try_set(&osink, "low-latency", "true");
            try_set(&osink, "sync", "false");
            pipeline.add_many([&oq, &oconv, &ores, &osink])?;
            gst::Element::link_many([&tee, &oq, &oconv, &ores, &osink])?;
        }

        Ok(Self { pipeline, src })
    }

    pub fn start(&self) -> crate::Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub fn stop(&self) -> crate::Result<()> {
        self.pipeline.set_state(gst::State::Null)?;
        Ok(())
    }

    /// Feeds one Opus packet. A gap in `pts_us` is what lets opusdec run
    /// packet loss concealment for the missing span.
    pub fn push(&self, data: &[u8], pts_us: u64) -> crate::Result<()> {
        let mut buf = gst::Buffer::from_slice(data.to_vec());
        {
            let b = buf.get_mut().expect("fresh buffer is writable");
            b.set_pts(gst::ClockTime::from_useconds(pts_us));
            b.set_duration(gst::ClockTime::from_mseconds(10));
        }
        self.src.push_buffer(buf)?;
        Ok(())
    }

    pub fn check_error(&self) -> crate::Result<()> {
        take_error(&self.pipeline)
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn live_appsrc(caps: &gst::Caps) -> AppSrc {
    AppSrc::builder()
        .caps(caps)
        .is_live(true)
        .format(gst::Format::Time)
        .stream_type(gst_app::AppStreamType::Stream)
        .max_bytes(4 * 1024 * 1024)
        .build()
}
