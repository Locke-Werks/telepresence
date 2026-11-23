use std::collections::HashMap;
use std::sync::Mutex;

use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};
use crate::identity::Fingerprint;

/// Persistence for pinned peers. The caller owns storage (a config file, the
/// registry, a database); this crate only asks and tells.
pub trait PeerStore: Send + Sync + 'static {
    fn is_pinned(&self, fingerprint: &Fingerprint) -> bool;
    /// Called once a pairing succeeds. `name` is the peer's self-reported name
    /// from its `Hello`, useful only for display.
    fn pin(&self, fingerprint: Fingerprint, name: &str);
    /// Called on a sender when a receiver it had pinned says it does not know
    /// this sender any more, so the next attempt asks for a code.
    fn unpin(&self, _fingerprint: &Fingerprint) {}
}

/// In-memory store, for tests and for callers that persist pins themselves.
#[derive(Debug, Default)]
pub struct MemoryPeerStore {
    peers: Mutex<HashMap<Fingerprint, String>>,
}

impl MemoryPeerStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn peers(&self) -> Vec<(Fingerprint, String)> {
        self.peers.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect()
    }
}

impl PeerStore for MemoryPeerStore {
    fn is_pinned(&self, fingerprint: &Fingerprint) -> bool {
        self.peers.lock().unwrap().contains_key(fingerprint)
    }

    fn pin(&self, fingerprint: Fingerprint, name: &str) {
        self.peers.lock().unwrap().insert(fingerprint, name.to_string());
    }

    fn unpin(&self, fingerprint: &Fingerprint) {
        self.peers.lock().unwrap().remove(fingerprint);
    }
}

/// Six random decimal digits for the receiver to display.
pub fn generate_pairing_code() -> String {
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32))
}

// Pairing runs SPAKE2 with the code as the password and both certificate
// fingerprints as the identities. A 6-digit code is far too small to survive an
// offline guess, and SPAKE2 is what prevents one: an eavesdropper or a man in
// the middle gets one online guess per attempt. Binding the fingerprints means
// the derived key only matches if both sides saw the same two certificates, so
// a relay that terminates TLS on either side fails confirmation.

pub(crate) struct PairingState {
    spake: Spake2<Ed25519Group>,
    sender_fp: Fingerprint,
    receiver_fp: Fingerprint,
}

pub(crate) fn start(code: &str, sender_fp: Fingerprint, receiver_fp: Fingerprint, is_sender: bool) -> (PairingState, Vec<u8>) {
    let pw = Password::new(code.trim().as_bytes());
    let a = SpakeId::new(&sender_fp.0);
    let b = SpakeId::new(&receiver_fp.0);
    let (spake, msg) = if is_sender {
        Spake2::<Ed25519Group>::start_a(&pw, &a, &b)
    } else {
        Spake2::<Ed25519Group>::start_b(&pw, &a, &b)
    };
    (PairingState { spake, sender_fp, receiver_fp }, msg)
}

pub(crate) struct PairingKey {
    key: Vec<u8>,
    sender_fp: Fingerprint,
    receiver_fp: Fingerprint,
}

impl PairingState {
    pub(crate) fn finish(self, peer_msg: &[u8]) -> Result<PairingKey> {
        let key = self.spake.finish(peer_msg).map_err(|_| Error::PairingFailed("bad SPAKE2 message"))?;
        Ok(PairingKey { key, sender_fp: self.sender_fp, receiver_fp: self.receiver_fp })
    }
}

impl PairingKey {
    /// Role label differs per side so one side's tag cannot be reflected back.
    pub(crate) fn confirm_tag(&self, from_sender: bool) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("any key length");
        mac.update(if from_sender { b"tp-confirm-sender" } else { b"tp-confirm-receiv" });
        mac.update(&self.sender_fp.0);
        mac.update(&self.receiver_fp.0);
        mac.finalize().into_bytes().to_vec()
    }

    pub(crate) fn verify(&self, from_sender: bool, tag: &[u8]) -> bool {
        bool::from(self.confirm_tag(from_sender).ct_eq(tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(b: u8) -> Fingerprint {
        Fingerprint([b; 32])
    }

    fn run(sender_code: &str, receiver_code: &str, s_view: (Fingerprint, Fingerprint), r_view: (Fingerprint, Fingerprint)) -> bool {
        let (s, sm) = start(sender_code, s_view.0, s_view.1, true);
        let (r, rm) = start(receiver_code, r_view.0, r_view.1, false);
        let sk = s.finish(&rm).unwrap();
        let rk = r.finish(&sm).unwrap();
        let r_tag = rk.confirm_tag(false);
        let s_tag = sk.confirm_tag(true);
        sk.verify(false, &r_tag) && rk.verify(true, &s_tag)
    }

    #[test]
    fn matching_code_confirms() {
        assert!(run("123456", "123456", (fp(1), fp(2)), (fp(1), fp(2))));
    }

    #[test]
    fn wrong_code_fails() {
        assert!(!run("123456", "123457", (fp(1), fp(2)), (fp(1), fp(2))));
    }

    #[test]
    fn relayed_certificates_fail() {
        // Sender sees the attacker's certificate (3) where the receiver's should be.
        assert!(!run("123456", "123456", (fp(1), fp(3)), (fp(1), fp(2))));
    }

    #[test]
    fn reflected_tag_fails() {
        let (s, sm) = start("000001", fp(1), fp(2), true);
        let (r, rm) = start("000001", fp(1), fp(2), false);
        let sk = s.finish(&rm).unwrap();
        let rk = r.finish(&sm).unwrap();
        assert!(!rk.verify(true, &rk.confirm_tag(false)));
        assert!(!sk.verify(false, &sk.confirm_tag(true)));
    }

    #[test]
    fn code_is_six_digits() {
        for _ in 0..100 {
            let c = generate_pairing_code();
            assert_eq!(c.len(), 6);
            assert!(c.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn memory_store_pins() {
        let s = MemoryPeerStore::new();
        assert!(!s.is_pinned(&fp(9)));
        s.pin(fp(9), "laptop");
        assert!(s.is_pinned(&fp(9)));
        assert_eq!(s.peers(), vec![(fp(9), "laptop".to_string())]);
    }
}
