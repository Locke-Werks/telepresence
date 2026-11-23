//! Shared-memory transport from the Receiver (single writer) to the virtual
//! camera and microphone drivers (readers).
//!
//! The byte layout is the contract with `drivers/common/tp_sink.h` and is
//! described in `docs/sink-format.md`. Any change to `layout.rs` must bump
//! [`layout::VIDEO_VERSION`] or [`layout::VERSION`] (audio) and be mirrored
//! in the C header.

pub mod audio;
pub mod layout;
mod shm;
pub mod video;

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub use audio::{AudioReadStats, AudioReader, AudioWriter};
pub use shm::unlink;
/// Security descriptor applied to sections created by the writer.
#[cfg(windows)]
pub use shm::SDDL as WINDOWS_SDDL;
pub use video::{FrameInfo, VideoRead, VideoReader, VideoWriter};

/// A heartbeat older than this means the writer is gone or wedged, and
/// drivers should present black video and silence.
pub const STALE_AFTER_US: u64 = 500_000;

/// Default object names. Windows uses the Global namespace so FrameServer,
/// which runs as LocalService in session 0, can see an object created from
/// the interactive session. macOS camera extensions are sandboxed and need an
/// app-group prefix, so callers there should build their own name.
#[cfg(windows)]
pub const DEFAULT_VIDEO_NAME: &str = r"Global\TelePresence.Sink.Video.v1";
#[cfg(windows)]
pub const DEFAULT_AUDIO_NAME: &str = r"Global\TelePresence.Sink.Audio.v1";
/// Largest frame the Windows camera sizes its section for. Pass these to
/// [`VideoWriter::open_existing`]; a larger maximum fails with `TooSmall`.
pub const VIDEO_MAX_WIDTH: u32 = 3840;
pub const VIDEO_MAX_HEIGHT: u32 = 2160;

#[cfg(unix)]
pub const DEFAULT_VIDEO_NAME: &str = "/tp.sink.video.v1";
#[cfg(unix)]
pub const DEFAULT_AUDIO_NAME: &str = "/tp.sink.audio.v1";

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    BadName,
    /// The region exists but has not been initialised by a writer yet.
    NotReady,
    BadMagic(u32),
    VersionMismatch(u32),
    /// The mapped object is smaller than its header claims or than needed.
    TooSmall { needed: usize, actual: usize },
    InvalidFormat(&'static str),
    CapacityExceeded,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "shared memory: {e}"),
            Error::BadName => f.write_str("invalid shared memory name"),
            Error::NotReady => f.write_str("region not initialised by a writer"),
            Error::BadMagic(m) => write!(f, "bad magic {m:#010x}"),
            Error::VersionMismatch(v) => write!(f, "unsupported layout version {v}"),
            Error::TooSmall { needed, actual } => {
                write!(f, "region too small: need {needed} bytes, have {actual}")
            }
            Error::InvalidFormat(why) => write!(f, "invalid format: {why}"),
            Error::CapacityExceeded => f.write_str("frame exceeds region capacity"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Wall-clock microseconds since the Unix epoch. Wall clock rather than a
/// monotonic source because the writer and readers are separate processes,
/// possibly under different users, and need a clock they all agree on.
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// True when a heartbeat is old enough that the writer should be treated as
/// absent. A heartbeat in the future (clock stepped back) counts as fresh so
/// an NTP correction does not blank the camera.
pub fn is_stale(heartbeat_us: u64, now: u64) -> bool {
    heartbeat_us == 0 || now.saturating_sub(heartbeat_us) > STALE_AFTER_US
}

fn current_pid() -> u32 {
    std::process::id()
}
