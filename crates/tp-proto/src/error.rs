use std::net::SocketAddr;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Cert(#[from] rcgen::Error),
    #[error("connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream write: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("stream read: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("datagram: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    #[error("encoding: {0}")]
    Encoding(#[from] postcard::Error),
    #[error("address {0} is not on a private, link-local or loopback network")]
    AddressNotAllowed(SocketAddr),
    #[error("peer is not paired and no pairing code was supplied")]
    NotPaired,
    #[error("pairing failed: {0}")]
    PairingFailed(&'static str),
    #[error("peer rejected the connection: {0}")]
    Rejected(String),
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("protocol version mismatch: local {local}, peer {peer}")]
    Version { local: u16, peer: u16 },
    #[error("frame too large to fragment ({0} bytes)")]
    FrameTooLarge(usize),
    #[error("peer does not support datagrams")]
    DatagramsUnsupported,
    #[error("timed out")]
    Timeout,
    #[error("endpoint closed")]
    Closed,
}

pub type Result<T> = std::result::Result<T, Error>;
