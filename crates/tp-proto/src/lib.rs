//! TelePresence transport: QUIC between a Sender and a Receiver on the same LAN.
//!
//! One reliable bidirectional stream carries length-prefixed postcard control
//! messages; media rides in QUIC datagrams with a fixed header, fragmented to
//! fit the path and reassembled with a deadline on the far side.
//!
//! Trust is established once by pairing with a 6-digit code shown on the
//! receiver (SPAKE2 bound to both certificate fingerprints), after which each
//! side pins the other's fingerprint through a caller-supplied [`PeerStore`].

mod addr;
mod control;
mod endpoint;
mod error;
mod fragment;
mod header;
mod identity;
mod pairing;
mod tls;

pub use addr::{is_allowed_addr, is_allowed_ip};
pub use control::{
    AudioCodec, AudioConfig, ControlMessage, MAX_CONTROL_FRAME, PROTOCOL_VERSION, VideoCodec, VideoConfig,
};
pub use endpoint::{Endpoint, EndpointConfig, MediaEvent, MediaFrame, Session, SessionStats};
pub use error::{Error, Result};
pub use fragment::{CompleteFrame, Fragmenter, ReassemblyOutcome, ReassemblyStats, Reassembler};
pub use header::{HEADER_LEN, HEADER_VERSION, HeaderError, MediaHeader, StreamKind};
pub use identity::{Fingerprint, Identity};
pub use pairing::{MemoryPeerStore, PeerStore, generate_pairing_code};
