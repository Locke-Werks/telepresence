/// Everything the core reports to a front end.
#[derive(Debug, Clone)]
pub enum Event {
    Status(String),
    /// Receiver is bound and accepting.
    Listening { port: u16, fingerprint: String },
    /// Receiver pairing code to display, or `None` once it is consumed or closed.
    PairingCode(Option<String>),
    /// Sender found, or refreshed, a receiver on the LAN.
    PeerFound(PeerInfo),
    PeerLost(String),
    Connected { peer: String, addr: String },
    Disconnected { reason: String },
    /// Sender reached a receiver that does not know it; a code is required.
    NeedsPairingCode,
    Stats(StatsSnapshot),
    /// The video actually flowing: the sender's negotiated encode format, or
    /// what the receiver was told and decodes. `fps` is 0 when unknown.
    VideoFormat { width: u32, height: u32, fps: u32 },
    /// Peak level of the most recent audio, 0.0 to 1.0.
    AudioLevel(f32),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    /// `ip:port`, best address first.
    pub addr: String,
    /// Hex certificate fingerprint the receiver advertises, for matching
    /// against this sender's own pins.
    pub fingerprint: String,
    /// The receiver has paired with some sender; not necessarily this one.
    pub paired: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StatsSnapshot {
    pub fps: f32,
    pub kbps: f32,
    pub loss: f32,
    pub rtt_ms: u32,
    /// Sender only: the encoder target the bitrate controller chose.
    pub target_kbps: u32,
}
