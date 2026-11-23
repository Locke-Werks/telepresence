use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};

/// Protocol version carried in `Hello`. Peers with different versions refuse
/// each other rather than guessing at compatibility.
pub const PROTOCOL_VERSION: u16 = 1;

/// Upper bound on one control frame. Control traffic is tiny; a larger length
/// prefix means a broken or hostile peer, and refusing it stops a 4 GiB
/// allocation.
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    /// Uncompressed or app-defined; both ends must agree out of band.
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus,
    PcmS16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: VideoCodec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub codec: AudioCodec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    Hello { version: u16, name: String },
    StreamConfig { video: Option<VideoConfig>, audio: Option<AudioConfig> },
    RequestKeyframe,
    Stats { loss: f32, rtt_ms: u32 },
    Bye,
}

/// Messages exchanged on the control stream before it is handed to the caller.
/// Kept separate from `ControlMessage` so callers can never send or receive
/// authentication frames by accident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AuthMessage {
    /// Sender claims both sides already pinned each other.
    Pinned,
    /// Sender starts pairing with its SPAKE2 message.
    PairStart { spake: Vec<u8> },
    /// Receiver's SPAKE2 message plus its key confirmation tag.
    PairReply { spake: Vec<u8>, confirm: Vec<u8> },
    /// Sender's key confirmation tag.
    PairConfirm { confirm: Vec<u8> },
    Accept,
    Reject { reason: String },
}

pub(crate) fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_CONTROL_FRAME {
        return Err(Error::Protocol("control frame too large"));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub(crate) async fn write_frame<T: Serialize>(s: &mut quinn::SendStream, msg: &T) -> Result<()> {
    s.write_all(&encode_frame(msg)?).await?;
    Ok(())
}

/// Returns `None` on a clean end of stream at a frame boundary.
pub(crate) async fn read_frame<T: DeserializeOwned>(r: &mut quinn::RecvStream) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_CONTROL_FRAME {
        return Err(Error::Protocol("control frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(postcard::from_bytes(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let msgs = [
            ControlMessage::Hello { version: PROTOCOL_VERSION, name: "desk cam".into() },
            ControlMessage::StreamConfig {
                video: Some(VideoConfig { width: 1920, height: 1080, fps: 30, codec: VideoCodec::H264 }),
                audio: Some(AudioConfig { sample_rate: 48_000, channels: 2, codec: AudioCodec::Opus }),
            },
            ControlMessage::RequestKeyframe,
            ControlMessage::Stats { loss: 0.25, rtt_ms: 3 },
            ControlMessage::Bye,
        ];
        for m in msgs {
            let f = encode_frame(&m).unwrap();
            let len = u32::from_be_bytes(f[..4].try_into().unwrap()) as usize;
            assert_eq!(len, f.len() - 4);
            let back: ControlMessage = postcard::from_bytes(&f[4..]).unwrap();
            assert_eq!(back, m);
        }
    }
}
