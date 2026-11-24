//! TelePresence application core: everything below the UI. The CLI and the Qt
//! front end both drive [`run_receiver`] and [`run_sender`] and read the same
//! [`Event`] stream, so neither front end owns any protocol logic.

pub mod abr;
pub mod devices;
// Lives in the library, not the binary: cxx-qt links its generated objects
// into every target of the package, so the bridge symbols must be in all of
// them, test binaries included.
#[cfg(feature = "gui")]
mod bridge;
#[cfg(feature = "gui")]
pub mod gui;
pub mod events;
pub mod receiver;
pub mod sender;
pub mod sink;
pub mod store;
pub mod update;

pub use events::{Event, PeerInfo, StatsSnapshot};
pub use receiver::{AudioOut, ReceiverConfig, run_receiver};
pub use sender::{SenderConfig, run_sender};
pub use sink::{AudioBlock, FrameSink, NullSink, ShmSink, VideoFrame};

pub const DEFAULT_PORT: u16 = 47800;

/// Machine name for `Hello` and the mDNS advertisement when the user gave none.
pub fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "TelePresence".to_string())
}
