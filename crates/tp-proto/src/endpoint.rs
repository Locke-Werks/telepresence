use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::pki_types::CertificateDer;
use tokio::sync::Mutex as AsyncMutex;

use crate::addr::is_allowed_addr;
use crate::control::{AuthMessage, ControlMessage, PROTOCOL_VERSION, read_frame, write_frame};
use crate::error::{Error, Result};
use crate::fragment::{Fragmenter, ReassemblyStats, Reassembler};
use crate::header::{MediaHeader, StreamKind};
use crate::identity::{Fingerprint, Identity};
use crate::pairing::{self, PeerStore};
use crate::tls;

/// A pairing code is abandoned after this many wrong guesses, so the 10^6 code
/// space cannot be walked online.
const MAX_PAIRING_FAILURES: u32 = 3;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a rejecting receiver waits for its Reject frame to be acknowledged
/// before closing. The close code carries the verdict regardless; the frame is
/// for peers that read it first.
const REJECT_LINGER: Duration = Duration::from_millis(200);

/// QUIC application close codes a receiver uses to refuse a handshake. A close
/// frame cannot be lost behind a dropped stream the way a Reject frame can, so
/// the sender learns the verdict even when it never reads the stream.
pub mod close_code {
    /// Generic handshake failure.
    pub const HANDSHAKE_FAILED: u32 = 1;
    pub const NOT_PAIRED: u32 = 0x10;
    pub const PAIRING_FAILED: u32 = 0x11;
    pub const NOT_IN_PAIRING_MODE: u32 = 0x12;
    pub const VERSION_MISMATCH: u32 = 0x13;
}

#[derive(Clone)]
pub struct EndpointConfig {
    pub identity: Identity,
    /// Human-readable name sent in `Hello`.
    pub name: String,
    pub peers: Arc<dyn PeerStore>,
    /// How long an incomplete frame waits for its missing fragments before it
    /// is dropped and reported as loss.
    pub reassembly_deadline: Duration,
}

impl EndpointConfig {
    pub fn new(identity: Identity, name: impl Into<String>, peers: Arc<dyn PeerStore>) -> Self {
        EndpointConfig { identity, name: name.into(), peers, reassembly_deadline: Duration::from_millis(100) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Sender,
    Receiver,
}

#[derive(Default)]
struct PairingSlot {
    code: Option<String>,
    failures: u32,
}

/// One side of the transport. Construct inside a tokio runtime.
pub struct Endpoint {
    inner: quinn::Endpoint,
    cfg: EndpointConfig,
    role: Role,
    pairing: Mutex<PairingSlot>,
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("in range")));
    // A 4K keyframe can be several hundred datagrams; the default send buffer
    // would silently drop the tail of it.
    t.datagram_send_buffer_size(8 * 1024 * 1024);
    t.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    // Only the control stream is ever opened.
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    Arc::new(t)
}

impl Endpoint {
    /// Client side. `bind` is usually `0.0.0.0:0` or `[::]:0`.
    pub fn sender(cfg: EndpointConfig, bind: SocketAddr) -> Result<Self> {
        let crypto = QuicClientConfig::try_from(tls::client_config(&cfg.identity)?)
            .map_err(|_| Error::Protocol("TLS config lacks a QUIC initial cipher suite"))?;
        let mut client = quinn::ClientConfig::new(Arc::new(crypto));
        client.transport_config(transport_config());
        let mut inner = quinn::Endpoint::client(bind)?;
        inner.set_default_client_config(client);
        Ok(Endpoint { inner, cfg, role: Role::Sender, pairing: Mutex::default() })
    }

    /// Server side, listening on `bind`.
    pub fn receiver(cfg: EndpointConfig, bind: SocketAddr) -> Result<Self> {
        let crypto = QuicServerConfig::try_from(tls::server_config(&cfg.identity)?)
            .map_err(|_| Error::Protocol("TLS config lacks a QUIC initial cipher suite"))?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        server.transport_config(transport_config());
        let inner = quinn::Endpoint::server(server, bind)?;
        Ok(Endpoint { inner, cfg, role: Role::Receiver, pairing: Mutex::default() })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.inner.local_addr()?)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.cfg.identity.fingerprint()
    }

    /// Receiver only: opens pairing with `code` (see `generate_pairing_code`), or
    /// closes it with `None`. A code is single use: it is cleared after one
    /// successful pairing or after repeated failures.
    pub fn set_pairing_code(&self, code: Option<String>) {
        let mut p = self.pairing.lock().unwrap();
        p.code = code;
        p.failures = 0;
    }

    pub fn pairing_open(&self) -> bool {
        self.pairing.lock().unwrap().code.is_some()
    }

    /// Sender only. With `pairing_code` set, pairs with the receiver and pins
    /// it; without, the receiver must already be pinned on both sides.
    pub async fn connect(&self, addr: SocketAddr, pairing_code: Option<&str>) -> Result<Session> {
        if self.role != Role::Sender {
            return Err(Error::Protocol("connect called on a receiver endpoint"));
        }
        if !is_allowed_addr(addr) {
            return Err(Error::AddressNotAllowed(addr));
        }
        let fut = async {
            let conn = self.inner.connect(addr, tls::SERVER_NAME)?.await?;
            let res = self.sender_handshake(&conn, pairing_code).await;
            match res {
                Ok(s) => Ok(s),
                Err(e) => {
                    // Whatever step noticed it, a receiver's application close
                    // is the real reason the handshake ended.
                    let e = rejection_from_error(&e)
                        .or_else(|| conn.close_reason().as_ref().and_then(rejection_from_close))
                        .unwrap_or(e);
                    // The receiver no longer knows us, so our pin of it is
                    // stale; keeping it would skip the code prompt forever.
                    if matches!(e, Error::NotPaired)
                        && pairing_code.is_none()
                        && let Ok(fp) = peer_fingerprint(&conn)
                        && self.cfg.peers.is_pinned(&fp)
                    {
                        self.cfg.peers.unpin(&fp);
                    }
                    conn.close(close_code::HANDSHAKE_FAILED.into(), e.to_string().as_bytes());
                    Err(e)
                }
            }
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, fut).await.map_err(|_| Error::Timeout)?
    }

    /// Receiver only. Waits for the next connection and authenticates it.
    /// Connections from non-LAN addresses are refused silently and never
    /// returned. A failed authentication is returned as an error; call again to
    /// keep listening.
    pub async fn accept(&self) -> Result<Session> {
        if self.role != Role::Receiver {
            return Err(Error::Protocol("accept called on a sender endpoint"));
        }
        loop {
            let incoming = self.inner.accept().await.ok_or(Error::Closed)?;
            let remote = incoming.remote_address();
            if !is_allowed_addr(remote) {
                log::warn!("refusing connection from non-LAN address {remote}");
                incoming.refuse();
                continue;
            }
            let fut = async {
                let conn = incoming.await?;
                let res = self.receiver_handshake(&conn).await;
                if let Err(e) = &res {
                    conn.close(close_code_for(e).into(), e.to_string().as_bytes());
                }
                res
            };
            return tokio::time::timeout(HANDSHAKE_TIMEOUT, fut).await.map_err(|_| Error::Timeout)?;
        }
    }

    /// Stops accepting and closes every connection.
    pub fn close(&self) {
        self.inner.close(0u32.into(), b"endpoint closed");
    }

    /// Waits until in-flight connections have finished closing.
    pub async fn wait_idle(&self) {
        self.inner.wait_idle().await;
    }

    async fn sender_handshake(&self, conn: &quinn::Connection, code: Option<&str>) -> Result<Session> {
        let peer_fp = peer_fingerprint(conn)?;
        let my_fp = self.fingerprint();
        let (mut send, mut recv) = conn.open_bi().await?;
        let paired = match code {
            None => {
                if !self.cfg.peers.is_pinned(&peer_fp) {
                    return Err(Error::NotPaired);
                }
                write_frame(&mut send, &AuthMessage::Pinned).await?;
                false
            }
            Some(code) => {
                let (state, msg) = pairing::start(code, my_fp, peer_fp, true);
                write_frame(&mut send, &AuthMessage::PairStart { spake: msg }).await?;
                let (spake, confirm) = match expect_auth(&mut recv).await? {
                    AuthMessage::PairReply { spake, confirm } => (spake, confirm),
                    AuthMessage::Reject { reason } => return Err(Error::Rejected(reason)),
                    _ => return Err(Error::Protocol("unexpected auth message")),
                };
                let key = state.finish(&spake)?;
                if !key.verify(false, &confirm) {
                    return Err(Error::PairingFailed("wrong code or certificate mismatch"));
                }
                write_frame(&mut send, &AuthMessage::PairConfirm { confirm: key.confirm_tag(true) }).await?;
                true
            }
        };
        match expect_auth(&mut recv).await? {
            AuthMessage::Accept => {}
            // The pinned path has one refusal: the receiver lost our pin.
            AuthMessage::Reject { .. } if !paired => return Err(Error::NotPaired),
            AuthMessage::Reject { reason } => return Err(Error::Rejected(reason)),
            _ => return Err(Error::Protocol("unexpected auth message")),
        }
        write_frame(&mut send, &self.hello()).await?;
        let (version, name) = expect_hello(&mut recv).await?;
        if paired {
            self.cfg.peers.pin(peer_fp, &name);
        }
        Ok(Session::new(conn.clone(), send, recv, peer_fp, name, version, self.cfg.reassembly_deadline))
    }

    async fn receiver_handshake(&self, conn: &quinn::Connection) -> Result<Session> {
        let peer_fp = peer_fingerprint(conn)?;
        let my_fp = self.fingerprint();
        let (mut send, mut recv) = conn.accept_bi().await?;
        let paired = match expect_auth(&mut recv).await? {
            AuthMessage::Pinned => {
                if !self.cfg.peers.is_pinned(&peer_fp) {
                    reject(&mut send, "not paired").await;
                    return Err(Error::NotPaired);
                }
                false
            }
            AuthMessage::PairStart { spake } => {
                let Some(code) = self.pairing.lock().unwrap().code.clone() else {
                    reject(&mut send, "receiver is not in pairing mode").await;
                    return Err(Error::PairingFailed(NOT_IN_PAIRING_MODE));
                };
                let (state, msg) = pairing::start(&code, peer_fp, my_fp, false);
                let key = state.finish(&spake)?;
                write_frame(&mut send, &AuthMessage::PairReply { spake: msg, confirm: key.confirm_tag(false) })
                    .await?;
                let confirm = match expect_auth(&mut recv).await? {
                    AuthMessage::PairConfirm { confirm } => confirm,
                    _ => return Err(Error::Protocol("unexpected auth message")),
                };
                if !key.verify(true, &confirm) {
                    self.note_pairing_failure();
                    reject(&mut send, "pairing failed").await;
                    return Err(Error::PairingFailed("wrong code or certificate mismatch"));
                }
                self.pairing.lock().unwrap().code = None;
                true
            }
            _ => return Err(Error::Protocol("unexpected auth message")),
        };
        write_frame(&mut send, &AuthMessage::Accept).await?;
        let (version, name) = expect_hello(&mut recv).await?;
        write_frame(&mut send, &self.hello()).await?;
        if paired {
            self.cfg.peers.pin(peer_fp, &name);
        }
        Ok(Session::new(conn.clone(), send, recv, peer_fp, name, version, self.cfg.reassembly_deadline))
    }

    fn note_pairing_failure(&self) {
        let mut p = self.pairing.lock().unwrap();
        p.failures += 1;
        if p.failures >= MAX_PAIRING_FAILURES {
            log::warn!("pairing code abandoned after {} failures", p.failures);
            p.code = None;
        }
    }

    fn hello(&self) -> ControlMessage {
        ControlMessage::Hello { version: PROTOCOL_VERSION, name: self.cfg.name.clone() }
    }
}

fn peer_fingerprint(conn: &quinn::Connection) -> Result<Fingerprint> {
    let certs = conn
        .peer_identity()
        .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
        .ok_or(Error::Protocol("peer presented no certificate"))?;
    let leaf = certs.first().ok_or(Error::Protocol("peer presented no certificate"))?;
    Ok(Fingerprint::of(leaf.as_ref()))
}

async fn expect_auth(recv: &mut quinn::RecvStream) -> Result<AuthMessage> {
    read_frame(recv).await?.ok_or(Error::Protocol("control stream closed during handshake"))
}

async fn expect_hello(recv: &mut quinn::RecvStream) -> Result<(u16, String)> {
    match read_frame::<ControlMessage>(recv).await? {
        Some(ControlMessage::Hello { version, name }) => {
            if version != PROTOCOL_VERSION {
                return Err(Error::Version { local: PROTOCOL_VERSION, peer: version });
            }
            Ok((version, name))
        }
        _ => Err(Error::Protocol("expected Hello")),
    }
}

async fn reject(send: &mut quinn::SendStream, reason: &str) {
    // Best effort: the application close that follows carries the verdict, so
    // this only gives a peer that is reading the stream the reason in words.
    if write_frame(send, &AuthMessage::Reject { reason: reason.to_string() }).await.is_ok() && send.finish().is_ok() {
        let _ = tokio::time::timeout(REJECT_LINGER, send.stopped()).await;
    }
}

/// The application close code a receiver sends for a failed handshake.
fn close_code_for(e: &Error) -> u32 {
    match e {
        Error::NotPaired => close_code::NOT_PAIRED,
        Error::PairingFailed(NOT_IN_PAIRING_MODE) => close_code::NOT_IN_PAIRING_MODE,
        Error::PairingFailed(_) => close_code::PAIRING_FAILED,
        Error::Version { .. } => close_code::VERSION_MISMATCH,
        _ => close_code::HANDSHAKE_FAILED,
    }
}

const NOT_IN_PAIRING_MODE: &str = "not in pairing mode";

/// Turns a receiver's application close into the error it stands for.
fn rejection_from_close(e: &quinn::ConnectionError) -> Option<Error> {
    let quinn::ConnectionError::ApplicationClosed(ac) = e else { return None };
    let code = u32::try_from(ac.error_code.into_inner()).ok()?;
    Some(match code {
        close_code::NOT_PAIRED => Error::NotPaired,
        close_code::PAIRING_FAILED => Error::PairingFailed("wrong code or certificate mismatch"),
        close_code::NOT_IN_PAIRING_MODE => Error::Rejected("receiver is not in pairing mode".into()),
        close_code::VERSION_MISMATCH => Error::Rejected("protocol version mismatch".into()),
        _ => return None,
    })
}

/// Finds a receiver's application close inside a stream or connection error.
fn rejection_from_error(e: &Error) -> Option<Error> {
    let conn = match e {
        Error::Connection(c) => c,
        Error::Read(quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(c))) => c,
        Error::Write(quinn::WriteError::ConnectionLost(c)) => c,
        _ => return None,
    };
    rejection_from_close(conn)
}

/// A reassembled media frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFrame {
    pub data: Bytes,
    /// Sender clock, microseconds.
    pub timestamp_us: u64,
    pub keyframe: bool,
    pub frame_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaEvent {
    Video(MediaFrame),
    Audio(MediaFrame),
    /// Frames of `kind` were abandoned. For video the caller should send
    /// `ControlMessage::RequestKeyframe`, since the decoder's reference chain
    /// is now broken.
    Loss { kind: StreamKind, frames: u32 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionStats {
    pub video: ReassemblyStats,
    pub audio: ReassemblyStats,
    pub rtt: Duration,
}

impl SessionStats {
    /// Combined datagram loss fraction, suitable for `ControlMessage::Stats`.
    pub fn loss(&self) -> f32 {
        let missing = self.video.datagrams_missing + self.audio.datagrams_missing;
        let total = missing + self.video.datagrams_received + self.audio.datagrams_received;
        if total == 0 { 0.0 } else { missing as f32 / total as f32 }
    }
}

struct RxState {
    video: Reassembler,
    audio: Reassembler,
    pending: VecDeque<MediaEvent>,
}

impl RxState {
    fn next_deadline(&self) -> Option<Instant> {
        match (self.video.next_deadline(), self.audio.next_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn expire(&mut self, now: Instant) {
        for kind in [StreamKind::Video, StreamKind::Audio] {
            let r = if kind == StreamKind::Video { &mut self.video } else { &mut self.audio };
            let lost = r.expire(now).lost;
            if lost > 0 {
                self.pending.push_back(MediaEvent::Loss { kind, frames: lost });
            }
        }
    }

    fn ingest(&mut self, dgram: &[u8], now: Instant) {
        let Ok((header, payload)) = MediaHeader::decode(dgram) else {
            log::debug!("dropping malformed datagram");
            return;
        };
        let r = match header.kind {
            StreamKind::Video => &mut self.video,
            StreamKind::Audio => &mut self.audio,
        };
        let (frame, outcome) = r.push(&header, payload, now);
        if outcome.lost > 0 {
            self.pending.push_back(MediaEvent::Loss { kind: header.kind, frames: outcome.lost });
        }
        if let Some(f) = frame {
            let mf = MediaFrame { data: f.data, timestamp_us: f.timestamp_us, keyframe: f.keyframe, frame_id: f.frame_id };
            self.pending.push_back(match f.kind {
                StreamKind::Video => MediaEvent::Video(mf),
                StreamKind::Audio => MediaEvent::Audio(mf),
            });
        }
    }
}

/// An authenticated connection. All methods take `&self`, so a session can be
/// shared in an `Arc` between a media task and a control task.
pub struct Session {
    conn: quinn::Connection,
    ctrl_send: AsyncMutex<quinn::SendStream>,
    ctrl_recv: AsyncMutex<quinn::RecvStream>,
    video_tx: Mutex<Fragmenter>,
    audio_tx: Mutex<Fragmenter>,
    rx: AsyncMutex<RxState>,
    peer_fp: Fingerprint,
    peer_name: String,
    peer_version: u16,
}

impl Session {
    fn new(
        conn: quinn::Connection,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        peer_fp: Fingerprint,
        peer_name: String,
        peer_version: u16,
        deadline: Duration,
    ) -> Self {
        Session {
            conn,
            ctrl_send: AsyncMutex::new(send),
            ctrl_recv: AsyncMutex::new(recv),
            video_tx: Mutex::new(Fragmenter::new(StreamKind::Video)),
            audio_tx: Mutex::new(Fragmenter::new(StreamKind::Audio)),
            rx: AsyncMutex::new(RxState {
                video: Reassembler::new(deadline),
                audio: Reassembler::new(deadline),
                pending: VecDeque::new(),
            }),
            peer_fp,
            peer_name,
            peer_version,
        }
    }

    pub fn peer_fingerprint(&self) -> Fingerprint {
        self.peer_fp
    }

    pub fn peer_name(&self) -> &str {
        &self.peer_name
    }

    pub fn peer_version(&self) -> u16 {
        self.peer_version
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Largest datagram the path currently allows, header included.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    pub fn send_video(&self, frame: &[u8], timestamp_us: u64, keyframe: bool) -> Result<()> {
        self.send_media(&self.video_tx, frame, timestamp_us, keyframe)
    }

    pub fn send_audio(&self, packet: &[u8], timestamp_us: u64) -> Result<()> {
        self.send_media(&self.audio_tx, packet, timestamp_us, false)
    }

    fn send_media(&self, tx: &Mutex<Fragmenter>, data: &[u8], ts: u64, key: bool) -> Result<()> {
        let max = self.conn.max_datagram_size().ok_or(Error::DatagramsUnsupported)?;
        let dgrams = tx.lock().unwrap().fragment(data, ts, key, max)?;
        for d in dgrams {
            self.conn.send_datagram(d)?;
        }
        Ok(())
    }

    /// Next reassembled frame or loss report. Returns an error once the
    /// connection is gone.
    pub async fn recv_media(&self) -> Result<MediaEvent> {
        let mut rx = self.rx.lock().await;
        loop {
            if let Some(ev) = rx.pending.pop_front() {
                return Ok(ev);
            }
            let deadline = rx.next_deadline();
            let sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                dg = self.conn.read_datagram() => {
                    let dg = dg?;
                    rx.ingest(&dg, Instant::now());
                }
                _ = sleep => rx.expire(Instant::now()),
            }
        }
    }

    pub async fn send_control(&self, msg: &ControlMessage) -> Result<()> {
        let mut s = self.ctrl_send.lock().await;
        write_frame(&mut s, msg).await
    }

    /// Next control message, or `None` when the peer finished the stream.
    pub async fn recv_control(&self) -> Result<Option<ControlMessage>> {
        let mut r = self.ctrl_recv.lock().await;
        read_frame(&mut r).await
    }

    pub async fn stats(&self) -> SessionStats {
        let rx = self.rx.lock().await;
        SessionStats { video: rx.video.stats(), audio: rx.audio.stats(), rtt: self.conn.rtt() }
    }

    /// Sends `Bye` and closes the connection.
    pub async fn close(&self) {
        let _ = self.send_control(&ControlMessage::Bye).await;
        let mut s = self.ctrl_send.lock().await;
        let _ = s.finish();
        // Closing the connection discards unacknowledged stream data, so give
        // the Bye a moment to land before tearing down.
        let _ = tokio::time::timeout(Duration::from_millis(500), s.stopped()).await;
        self.conn.close(0u32.into(), b"bye");
    }

    /// Resolves when the connection ends for any reason.
    pub async fn closed(&self) -> quinn::ConnectionError {
        self.conn.closed().await
    }
}
