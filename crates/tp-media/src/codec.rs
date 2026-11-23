use gst::prelude::*;

use crate::util::{make, try_set};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Encoder {
    Nvenc,
    Qsv,
    MediaFoundation,
    VideoToolbox,
    Vaapi,
    X264,
}

impl H264Encoder {
    /// Hardware first, software last. Media Foundation, VideoToolbox and VA-API
    /// are each the native fallback on one OS, so they share a tier.
    pub const PREFERENCE: [H264Encoder; 6] = [
        H264Encoder::Nvenc,
        H264Encoder::Qsv,
        H264Encoder::MediaFoundation,
        H264Encoder::VideoToolbox,
        H264Encoder::Vaapi,
        H264Encoder::X264,
    ];

    pub fn factory_name(self) -> &'static str {
        match self {
            H264Encoder::Nvenc => "nvh264enc",
            H264Encoder::Qsv => "qsvh264enc",
            H264Encoder::MediaFoundation => "mfh264enc",
            H264Encoder::VideoToolbox => "vtenc_h264",
            H264Encoder::Vaapi => "vaapih264enc",
            H264Encoder::X264 => "x264enc",
        }
    }

    pub(crate) fn build(self, bitrate_kbps: u32, fps: u32) -> crate::Result<gst::Element> {
        let enc = make(self.factory_name())?;
        // Keyframes are requested on loss and at stream start, so the periodic
        // one is only a safety net. A 2 s cadence made a visible hitch every
        // keyframe as CBR starved the frames around each large IDR.
        let gop = (fps.max(1) * 10).to_string();
        match self {
            H264Encoder::Nvenc => {
                // New nvcodec encoder uses p1..p7 + tune; the legacy one only knows
                // the low-latency-* presets. Try the modern names first.
                // p3 is still well under a frame of latency on any NVENC part
                // and visibly cleaner than p1 at the same bitrate.
                if !try_set(&enc, "preset", "p3") {
                    try_set(&enc, "preset", "low-latency-hp");
                }
                try_set(&enc, "tune", "ultra-low-latency");
                try_set(&enc, "rc-mode", "cbr");
                try_set(&enc, "zerolatency", "true");
                try_set(&enc, "zero-reorder-delay", "true");
                try_set(&enc, "bframes", "0");
                try_set(&enc, "gop-size", &gop);
            }
            H264Encoder::Qsv => {
                try_set(&enc, "low-latency", "true");
                try_set(&enc, "rate-control", "cbr");
                try_set(&enc, "b-frames", "0");
                try_set(&enc, "gop-size", &gop);
            }
            H264Encoder::MediaFoundation => {
                try_set(&enc, "low-latency", "true");
                try_set(&enc, "rc-mode", "cbr");
                try_set(&enc, "bframes", "0");
                try_set(&enc, "gop-size", &gop);
            }
            H264Encoder::VideoToolbox => {
                try_set(&enc, "realtime", "true");
                try_set(&enc, "allow-frame-reordering", "false");
                try_set(&enc, "max-keyframe-interval", &gop);
                try_set(&enc, "max-keyframe-interval-duration", "10000000000");
                // In realtime ABR mode VideoToolbox treats the bitrate as a
                // ceiling and undershoots it by 10x on camera content, which is
                // the soft picture seen in testing. CBR makes it spend the budget.
                try_set(&enc, "rate-control", "cbr");
                try_set(&enc, "quality", "0.75");
            }
            H264Encoder::Vaapi => {
                try_set(&enc, "rate-control", "cbr");
                try_set(&enc, "max-bframes", "0");
                try_set(&enc, "keyframe-period", &gop);
            }
            H264Encoder::X264 => {
                try_set(&enc, "tune", "zerolatency");
                try_set(&enc, "speed-preset", "ultrafast");
                try_set(&enc, "bframes", "0");
                try_set(&enc, "key-int-max", &gop);
            }
        }
        self.set_bitrate(&enc, bitrate_kbps);
        Ok(enc)
    }

    /// Every supported encoder takes kbit/s in `bitrate` and accepts a change
    /// while playing. No VideoToolbox data-rate cap: a 1 s window cannot fit
    /// an IDR, so the encoder starved the frames after each keyframe, and on a
    /// LAN a brief overshoot costs nothing.
    pub(crate) fn set_bitrate(self, enc: &gst::Element, kbps: u32) -> bool {
        try_set(enc, "bitrate", &kbps.to_string())
    }
}

/// Pure selection logic, separated from the registry so it can be tested.
pub fn select_encoder_with(is_available: impl Fn(&str) -> bool) -> Option<H264Encoder> {
    H264Encoder::PREFERENCE
        .into_iter()
        .find(|e| is_available(e.factory_name()))
}

/// A factory can be registered yet fail to instantiate (driver missing), so
/// availability means the element actually constructs.
pub fn select_encoder() -> Option<H264Encoder> {
    crate::init().ok()?;
    select_encoder_with(can_instantiate)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Decoder {
    D3D11,
    Nvdec,
    VideoToolbox,
    Va,
    Libav,
}

impl H264Decoder {
    pub const PREFERENCE: [H264Decoder; 5] = [
        H264Decoder::D3D11,
        H264Decoder::Nvdec,
        H264Decoder::VideoToolbox,
        H264Decoder::Va,
        H264Decoder::Libav,
    ];

    pub fn factory_name(self) -> &'static str {
        match self {
            H264Decoder::D3D11 => "d3d11h264dec",
            H264Decoder::Nvdec => "nvh264dec",
            H264Decoder::VideoToolbox => "vtdec",
            H264Decoder::Va => "vah264dec",
            H264Decoder::Libav => "avdec_h264",
        }
    }
}

pub fn select_decoder_with(is_available: impl Fn(&str) -> bool) -> Option<H264Decoder> {
    H264Decoder::PREFERENCE
        .into_iter()
        .find(|d| is_available(d.factory_name()))
}

pub fn select_decoder() -> Option<H264Decoder> {
    crate::init().ok()?;
    select_decoder_with(can_instantiate)
}

fn can_instantiate(factory: &str) -> bool {
    crate::util::element_exists(factory) && gst::ElementFactory::make(factory).build().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_prefers_nvenc_when_everything_present() {
        assert_eq!(select_encoder_with(|_| true), Some(H264Encoder::Nvenc));
    }

    #[test]
    fn encoder_falls_through_in_order() {
        let have = |set: &'static [&'static str]| move |n: &str| set.contains(&n);
        assert_eq!(
            select_encoder_with(have(&["x264enc", "qsvh264enc"])),
            Some(H264Encoder::Qsv)
        );
        assert_eq!(
            select_encoder_with(have(&["x264enc", "mfh264enc"])),
            Some(H264Encoder::MediaFoundation)
        );
        assert_eq!(
            select_encoder_with(have(&["x264enc", "vtenc_h264"])),
            Some(H264Encoder::VideoToolbox)
        );
        assert_eq!(
            select_encoder_with(have(&["vaapih264enc", "x264enc"])),
            Some(H264Encoder::Vaapi)
        );
        assert_eq!(
            select_encoder_with(have(&["x264enc"])),
            Some(H264Encoder::X264)
        );
        assert_eq!(select_encoder_with(have(&[])), None);
    }

    #[test]
    fn decoder_falls_through_in_order() {
        assert_eq!(select_decoder_with(|_| true), Some(H264Decoder::D3D11));
        assert_eq!(
            select_decoder_with(|n| n == "avdec_h264" || n == "nvh264dec"),
            Some(H264Decoder::Nvdec)
        );
        assert_eq!(
            select_decoder_with(|n| n == "avdec_h264"),
            Some(H264Decoder::Libav)
        );
    }

    #[test]
    fn report_selection_on_this_machine() {
        let enc = select_encoder();
        let dec = select_decoder();
        println!("selected encoder: {:?}", enc.map(|e| e.factory_name()));
        println!("selected decoder: {:?}", dec.map(|d| d.factory_name()));
    }
}
