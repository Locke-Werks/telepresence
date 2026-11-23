//! Media capture, encode, decode and playback for TelePresence.
//!
//! Transport-agnostic: the sender hands out encoded H.264 access units and
//! Opus packets, the receiver accepts them back. Moving the bytes is the
//! caller's job.

pub mod codec;
pub mod devices;
pub mod mode;
pub mod receiver;
pub mod sender;
mod util;

pub use codec::{
    H264Decoder, H264Encoder, select_decoder, select_decoder_with, select_encoder,
    select_encoder_with,
};
pub use devices::{
    DeviceInfo, DeviceKind, find_audio_output, find_device, find_vb_cable_input, list_devices,
};
pub use mode::{CameraMode, ModeFormat, ModeOverride, choose_mode, default_bitrate_kbps};
pub use receiver::{
    AudioOutput, AudioReceiver, AudioReceiverConfig, DecodedAudio, DecodedFrame, VideoReceiver,
    VideoReceiverConfig,
};
pub use sender::{
    AudioSource, EncodedAudio, EncodedVideo, Sender, SenderConfig, VideoFormat, VideoSource,
};

pub use gst;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("gstreamer init failed: {0}")]
    Init(#[from] gst::glib::Error),
    #[error("no usable {0} element found")]
    NoElement(&'static str),
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("pipeline construction failed: {0}")]
    Build(#[from] gst::glib::BoolError),
    #[error("state change failed: {0}")]
    State(#[from] gst::StateChangeError),
    #[error("pipeline error from {source_name}: {message}")]
    Pipeline {
        source_name: String,
        message: String,
    },
    #[error("push failed: {0}")]
    Flow(#[from] gst::FlowError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Safe to call repeatedly; every constructor in this crate calls it.
pub fn init() -> Result<()> {
    gst::init()?;
    Ok(())
}
