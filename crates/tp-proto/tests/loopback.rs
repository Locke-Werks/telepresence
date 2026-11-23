use std::sync::Arc;
use std::time::Duration;

use tp_proto::{
    AudioCodec, AudioConfig, ControlMessage, Endpoint, EndpointConfig, Error, Identity, MediaEvent, MemoryPeerStore,
    PeerStore, VideoCodec, VideoConfig, generate_pairing_code,
};

fn endpoints() -> (Endpoint, Endpoint, Arc<MemoryPeerStore>, Arc<MemoryPeerStore>) {
    let rx_store = Arc::new(MemoryPeerStore::new());
    let tx_store = Arc::new(MemoryPeerStore::new());
    let rx = Endpoint::receiver(
        EndpointConfig::new(Identity::generate().unwrap(), "receiver", rx_store.clone()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let tx = Endpoint::sender(
        EndpointConfig::new(Identity::generate().unwrap(), "sender", tx_store.clone()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    (rx, tx, rx_store, tx_store)
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f).await.expect("timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_then_reconnect_and_stream() {
    let (rx, tx, rx_store, tx_store) = endpoints();
    let addr = rx.local_addr().unwrap();

    // Unpaired connection without a code is refused before any data flows.
    let (a, c) = within(async { tokio::join!(rx.accept(), tx.connect(addr, None)) }).await;
    assert!(matches!(c, Err(Error::NotPaired)));
    assert!(a.is_err());

    // Wrong code fails and pins nothing.
    let code = generate_pairing_code();
    rx.set_pairing_code(Some(code.clone()));
    let wrong = if code == "000000" { "000001" } else { "000000" };
    let (a, c) = tokio::join!(rx.accept(), tx.connect(addr, Some(wrong)));
    assert!(a.is_err() && c.is_err());
    assert!(rx_store.peers().is_empty() && tx_store.peers().is_empty());

    // Right code pairs and pins on both sides.
    let (a, c) = within(async { tokio::join!(rx.accept(), tx.connect(addr, Some(&code))) }).await;
    let (rs, ss) = (a.unwrap(), c.unwrap());
    assert_eq!(rs.peer_name(), "sender");
    assert_eq!(ss.peer_name(), "receiver");
    assert!(rx_store.is_pinned(&tx.fingerprint()));
    assert!(tx_store.is_pinned(&rx.fingerprint()));
    assert!(!rx.pairing_open(), "code must be single use");
    ss.close().await;
    drop((rs, ss));

    // Reconnect on the pinned fingerprint alone.
    let (a, c) = within(async { tokio::join!(rx.accept(), tx.connect(addr, None)) }).await;
    let (rs, ss) = (a.unwrap(), c.unwrap());
    assert_eq!(rs.peer_fingerprint(), tx.fingerprint());
    assert_eq!(ss.peer_fingerprint(), rx.fingerprint());

    // Control round trip.
    let cfg = ControlMessage::StreamConfig {
        video: Some(VideoConfig { width: 1280, height: 720, fps: 30, codec: VideoCodec::H264 }),
        audio: Some(AudioConfig { sample_rate: 48_000, channels: 1, codec: AudioCodec::Opus }),
    };
    ss.send_control(&cfg).await.unwrap();
    assert_eq!(within(rs.recv_control()).await.unwrap(), Some(cfg));
    rs.send_control(&ControlMessage::RequestKeyframe).await.unwrap();
    assert_eq!(within(ss.recv_control()).await.unwrap(), Some(ControlMessage::RequestKeyframe));

    // Media round trip: a video frame large enough to need many fragments, and
    // an audio packet.
    let max = ss.max_datagram_size().expect("datagrams enabled");
    let video: Vec<u8> = (0..max * 7 + 123).map(|i| (i % 251) as u8).collect();
    ss.send_video(&video, 1_000_000, true).unwrap();
    ss.send_audio(b"opus-packet", 1_000_500).unwrap();

    let mut got_video = None;
    let mut got_audio = None;
    while got_video.is_none() || got_audio.is_none() {
        match within(rs.recv_media()).await.unwrap() {
            MediaEvent::Video(f) => got_video = Some(f),
            MediaEvent::Audio(f) => got_audio = Some(f),
            MediaEvent::Loss { kind, frames } => panic!("unexpected loss on loopback: {kind:?} {frames}"),
        }
    }
    let v = got_video.unwrap();
    assert_eq!(&v.data[..], &video[..]);
    assert_eq!(v.timestamp_us, 1_000_000);
    assert!(v.keyframe);
    let a = got_audio.unwrap();
    assert_eq!(&a.data[..], b"opus-packet");
    assert_eq!(a.timestamp_us, 1_000_500);

    let stats = rs.stats().await;
    assert_eq!(stats.loss(), 0.0);

    ss.close().await;
    assert_eq!(within(rs.recv_control()).await.unwrap(), Some(ControlMessage::Bye));
}

#[tokio::test]
async fn sender_refuses_public_address() {
    let tx = Endpoint::sender(
        EndpointConfig::new(Identity::generate().unwrap(), "s", Arc::new(MemoryPeerStore::new())),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let r = tx.connect("8.8.8.8:4433".parse().unwrap(), None).await;
    assert!(matches!(r, Err(Error::AddressNotAllowed(_))));
}

/// The receiver lost its pin of the sender (a wiped or clobbered peer file)
/// while the sender still pins the receiver. The sender must hear "not paired"
/// every time, not a generic connection loss, or it never offers a code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_sender_pin_is_reported_as_not_paired() {
    for round in 0..20 {
        let (rx, tx, rx_store, tx_store) = endpoints();
        let addr = rx.local_addr().unwrap();
        tx_store.pin(rx.fingerprint(), "receiver");
        let (a, c) = within(async { tokio::join!(rx.accept(), tx.connect(addr, None)) }).await;
        assert!(matches!(a, Err(Error::NotPaired)), "round {round}: receiver got {:?}", a.err());
        assert!(matches!(c, Err(Error::NotPaired)), "round {round}: sender got {:?}", c.err());
        assert!(rx_store.peers().is_empty());
        assert!(!tx_store.is_pinned(&rx.fingerprint()), "round {round}: stale pin kept");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_without_pairing_mode_is_rejected_by_reason() {
    let (rx, tx, _, _) = endpoints();
    let addr = rx.local_addr().unwrap();
    let (_, c) = within(async { tokio::join!(rx.accept(), tx.connect(addr, Some("123456"))) }).await;
    match c {
        Err(Error::Rejected(r)) => assert!(r.contains("pairing mode"), "{r}"),
        other => panic!("expected Rejected, got {:?}", other.err()),
    }
}
