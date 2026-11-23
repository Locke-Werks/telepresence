//! LAN discovery for TelePresence over mDNS / DNS-SD.
//!
//! Receivers advertise `_telepresence._udp.local.` with their QUIC port and a
//! small TXT record. Senders browse for that type and get a stream of
//! [`DiscoveryEvent`]s keyed by the receiver's stable install id.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use sha2::{Digest, Sha256};

pub const SERVICE_TYPE: &str = "_telepresence._udp.local.";
pub const PROTO_VERSION: u16 = 1;

const KEY_VERSION: &str = "v";
const KEY_ID: &str = "id";
const KEY_NAME: &str = "n";
const KEY_FP: &str = "fp";
const KEY_PAIRED: &str = "paired";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("mdns: {0}")]
    Mdns(#[from] mdns_sd::Error),
    #[error("invalid TXT record: {0}")]
    Txt(&'static str),
    #[error("invalid advertisement: {0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Hex SHA-256 of a DER certificate, the form carried in the `fp` TXT key.
pub fn fingerprint_hex(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

/// Decoded TXT payload of a receiver advertisement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtRecord {
    pub version: u16,
    pub install_id: String,
    pub name: String,
    /// Lowercase hex SHA-256 of the receiver's certificate.
    pub fingerprint: String,
    /// Hint only: a sender must still verify the fingerprint it pinned.
    pub paired: bool,
}

impl TxtRecord {
    pub fn encode(&self) -> Vec<(String, String)> {
        vec![
            (KEY_VERSION.into(), self.version.to_string()),
            (KEY_ID.into(), self.install_id.clone()),
            (KEY_NAME.into(), self.name.clone()),
            (KEY_FP.into(), self.fingerprint.clone()),
            (KEY_PAIRED.into(), if self.paired { "1" } else { "0" }.into()),
        ]
    }

    /// Decodes from a key lookup so it works against both raw pairs and
    /// mdns-sd's `TxtProperties` without copying.
    pub fn decode<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self> {
        let version = get(KEY_VERSION)
            .ok_or(Error::Txt("missing v"))?
            .parse()
            .map_err(|_| Error::Txt("bad v"))?;
        let install_id = get(KEY_ID).ok_or(Error::Txt("missing id"))?.to_string();
        if install_id.is_empty() {
            return Err(Error::Txt("empty id"));
        }
        let fingerprint = get(KEY_FP).ok_or(Error::Txt("missing fp"))?.to_ascii_lowercase();
        if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Txt("fp is not hex sha-256"));
        }
        let name = get(KEY_NAME).unwrap_or_default().to_string();
        let paired = matches!(get(KEY_PAIRED), Some("1" | "true"));
        Ok(Self { version, install_id, name, fingerprint, paired })
    }

    pub fn decode_pairs(pairs: &[(String, String)]) -> Result<Self> {
        Self::decode(|k| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.as_str()))
    }
}

/// What a receiver publishes.
#[derive(Debug, Clone)]
pub struct Advertisement {
    /// User-visible machine name.
    pub name: String,
    pub port: u16,
    pub install_id: String,
    pub fingerprint: String,
    pub paired: bool,
}

/// Live mDNS registration. Unregisters (sends goodbye packets) on drop.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    pub fn start(ad: &Advertisement) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        Self::start_with(daemon, ad)
    }

    fn start_with(daemon: ServiceDaemon, ad: &Advertisement) -> Result<Self> {
        if ad.install_id.is_empty() || !ad.install_id.is_ascii() {
            return Err(Error::Invalid("install id must be non-empty ascii"));
        }
        let txt = TxtRecord {
            version: PROTO_VERSION,
            install_id: ad.install_id.clone(),
            name: ad.name.clone(),
            fingerprint: ad.fingerprint.to_ascii_lowercase(),
            paired: ad.paired,
        };
        // Instance names must be unique on the link; two machines called
        // "Office PC" would otherwise fight over one record. The display name
        // travels in TXT so the suffix never reaches the UI.
        let short: String = ad.install_id.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
        let instance = format!("{}-{}", sanitize_label(&ad.name), short);
        let host = format!("tp-{short}.local.");
        let props: Vec<(String, String)> = txt.encode();
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, (), ad.port, &props[..])?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }

    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Wait for the goodbye to go out; shutting the daemon down first would
        // leave peers holding a stale record until its TTL expires.
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
        if let Ok(rx) = self.daemon.shutdown() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

fn sanitize_label(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
    if s.trim().is_empty() { "TelePresence".into() } else { s.trim().to_string() }
}

/// A discovered receiver, merged across every interface it answered on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Stable install id; the dedupe key.
    pub id: String,
    pub name: String,
    /// Best first: private IPv4, other IPv4, routable IPv6, link-local IPv6.
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    pub fingerprint: String,
    pub version: u16,
    pub paired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Found(Peer),
    Updated(Peer),
    Lost(String),
}

/// Ranks addresses so callers can simply try `addrs[0]` first.
pub fn addr_rank(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) if v4.is_private() => 0,
        IpAddr::V4(v4) if v4.is_link_local() => 2,
        IpAddr::V4(v4) if v4.is_loopback() => 4,
        IpAddr::V4(_) => 1,
        IpAddr::V6(v6) if v6.is_loopback() => 5,
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => 6,
        IpAddr::V6(_) => 3,
    }
}

pub fn sort_addrs(addrs: &mut Vec<IpAddr>) {
    addrs.sort_by_key(|a| (addr_rank(a), *a));
    addrs.dedup();
}

/// Browses for receivers. Stops browsing and shuts its daemon down on drop.
pub struct Browser {
    daemon: ServiceDaemon,
    rx: mpsc::Receiver<DiscoveryEvent>,
    worker: Option<JoinHandle<()>>,
}

impl Browser {
    pub fn start() -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let events = daemon.browse(SERVICE_TYPE)?;
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("tp-discovery".into())
            .spawn(move || {
                let mut tracker = Tracker::default();
                while let Ok(ev) = events.recv() {
                    for out in tracker.handle(ev) {
                        if tx.send(out).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("spawn discovery thread");
        Ok(Self { daemon, rx, worker: Some(worker) })
    }

    pub fn recv(&self) -> Option<DiscoveryEvent> {
        self.rx.recv().ok()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<DiscoveryEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    pub fn try_recv(&self) -> Option<DiscoveryEvent> {
        self.rx.try_recv().ok()
    }

    /// The underlying channel, for callers that want to select over it.
    pub fn receiver(&self) -> &mpsc::Receiver<DiscoveryEvent> {
        &self.rx
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        if let Ok(rx) = self.daemon.shutdown() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
        // The daemon closing its channel is what ends the worker loop.
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Folds raw mDNS events into per-install-id peers. Separate from the thread
/// so it can be driven deterministically in tests.
#[derive(Default)]
struct Tracker {
    /// mDNS fullname to install id. One install can appear under several
    /// fullnames if it was renamed or re-registered.
    names: HashMap<String, String>,
    /// Per install id: the per-fullname record plus the merged peer last emitted.
    peers: HashMap<String, (HashMap<String, Peer>, Peer)>,
}

impl Tracker {
    fn handle(&mut self, ev: ServiceEvent) -> Vec<DiscoveryEvent> {
        match ev {
            ServiceEvent::ServiceResolved(svc) => {
                let Ok(txt) = TxtRecord::decode(|k| svc.get_property_val_str(k)) else {
                    return Vec::new();
                };
                let addrs: Vec<IpAddr> = svc.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                let name = if txt.name.is_empty() {
                    mdns_sd::instance_name_unescaped(svc.get_fullname())
                } else {
                    txt.name.clone()
                };
                let peer = Peer {
                    id: txt.install_id,
                    name,
                    addrs,
                    port: svc.port,
                    fingerprint: txt.fingerprint,
                    version: txt.version,
                    paired: txt.paired,
                };
                self.upsert(svc.get_fullname().to_string(), peer)
            }
            ServiceEvent::ServiceRemoved(_, fullname) => self.remove(&fullname),
            _ => Vec::new(),
        }
    }

    fn upsert(&mut self, fullname: String, peer: Peer) -> Vec<DiscoveryEvent> {
        let mut out = Vec::new();
        // A fullname that now carries a different id means the old install
        // went away under that name.
        if let Some(old) = self.names.get(&fullname).cloned()
            && old != peer.id
        {
            out.extend(self.remove(&fullname));
        }
        self.names.insert(fullname.clone(), peer.id.clone());
        let id = peer.id.clone();
        let fresh = !self.peers.contains_key(&id);
        let entry = self.peers.entry(id).or_insert_with(|| (HashMap::new(), peer.clone()));
        entry.0.insert(fullname.clone(), peer);
        let merged = merge(&entry.0, &fullname);
        if fresh {
            entry.1 = merged.clone();
            out.push(DiscoveryEvent::Found(merged));
        } else if entry.1 != merged {
            entry.1 = merged.clone();
            out.push(DiscoveryEvent::Updated(merged));
        }
        out
    }

    fn remove(&mut self, fullname: &str) -> Vec<DiscoveryEvent> {
        let Some(id) = self.names.remove(fullname) else {
            return Vec::new();
        };
        let Some(entry) = self.peers.get_mut(&id) else {
            return Vec::new();
        };
        entry.0.remove(fullname);
        if entry.0.is_empty() {
            self.peers.remove(&id);
            return vec![DiscoveryEvent::Lost(id)];
        }
        let latest = entry.0.keys().next().cloned().unwrap_or_default();
        let merged = merge(&entry.0, &latest);
        if entry.1 != merged {
            entry.1 = merged.clone();
            vec![DiscoveryEvent::Updated(merged)]
        } else {
            Vec::new()
        }
    }
}

/// Metadata from the most recently resolved record, addresses from all.
fn merge(records: &HashMap<String, Peer>, latest: &str) -> Peer {
    let mut base = records
        .get(latest)
        .or_else(|| records.values().next())
        .cloned()
        .expect("merge on empty record set");
    let all: BTreeSet<IpAddr> = records.values().flat_map(|p| p.addrs.iter().copied()).collect();
    base.addrs = all.into_iter().collect();
    sort_addrs(&mut base.addrs);
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn fp() -> String {
        fingerprint_hex(b"test cert")
    }

    #[test]
    fn txt_roundtrip() {
        let rec = TxtRecord {
            version: PROTO_VERSION,
            install_id: "0123456789abcdef".into(),
            name: "Office PC".into(),
            fingerprint: fp(),
            paired: true,
        };
        let back = TxtRecord::decode_pairs(&rec.encode()).unwrap();
        assert_eq!(rec, back);
        let unpaired = TxtRecord { paired: false, ..rec };
        assert_eq!(TxtRecord::decode_pairs(&unpaired.encode()).unwrap(), unpaired);
    }

    #[test]
    fn txt_rejects_bad_input() {
        let mut pairs = TxtRecord {
            version: 1,
            install_id: "x".into(),
            name: String::new(),
            fingerprint: fp(),
            paired: false,
        }
        .encode();
        pairs.retain(|(k, _)| k != KEY_ID);
        assert!(TxtRecord::decode_pairs(&pairs).is_err());
        let bad_fp = vec![
            ("v".to_string(), "1".to_string()),
            ("id".to_string(), "x".to_string()),
            ("fp".to_string(), "zz".to_string()),
        ];
        assert!(TxtRecord::decode_pairs(&bad_fp).is_err());
    }

    #[test]
    fn fingerprint_is_hex_sha256() {
        let f = fp();
        assert_eq!(f.len(), 64);
        assert!(f.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[test]
    fn addr_ordering_prefers_private_v4() {
        let mut addrs = vec![
            IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
        ];
        sort_addrs(&mut addrs);
        assert_eq!(addrs.len(), 4);
        assert_eq!(addrs[0], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)));
        assert_eq!(addrs[1], IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(matches!(addrs[3], IpAddr::V6(v) if v.segments()[0] == 0xfe80));
    }

    fn peer(id: &str, addr: [u8; 4]) -> Peer {
        Peer {
            id: id.into(),
            name: "Box".into(),
            addrs: vec![IpAddr::V4(Ipv4Addr::from(addr))],
            port: 4433,
            fingerprint: fp(),
            version: 1,
            paired: false,
        }
    }

    #[test]
    fn tracker_dedupes_by_install_id() {
        let mut t = Tracker::default();
        let ev = t.upsert("a._telepresence._udp.local.".into(), peer("id1", [10, 0, 0, 5]));
        assert!(matches!(&ev[..], [DiscoveryEvent::Found(p)] if p.id == "id1"));
        // Same record again on another interface: nothing new.
        assert!(t.upsert("a._telepresence._udp.local.".into(), peer("id1", [10, 0, 0, 5])).is_empty());
        // Same install under a second fullname with another address: merged.
        let ev = t.upsert("b._telepresence._udp.local.".into(), peer("id1", [192, 168, 0, 9]));
        match &ev[..] {
            [DiscoveryEvent::Updated(p)] => assert_eq!(p.addrs.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(&t.remove("a._telepresence._udp.local.")[..], [DiscoveryEvent::Updated(_)]));
        assert_eq!(t.remove("b._telepresence._udp.local."), vec![DiscoveryEvent::Lost("id1".into())]);
        assert!(t.remove("b._telepresence._udp.local.").is_empty());
    }

    #[test]
    fn loopback_advertise_and_discover() {
        let id = format!("test{:016x}", std::process::id() as u64 ^ 0x5eed_1234_abcd);
        let browser = Browser::start().expect("browser");
        let ad = Advertisement {
            name: "Loopback Test".into(),
            port: 45_123,
            install_id: id.clone(),
            fingerprint: fp(),
            paired: false,
        };
        let adv = Advertiser::start(&ad).expect("advertiser");

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut found = None;
        while std::time::Instant::now() < deadline {
            if let Some(DiscoveryEvent::Found(p) | DiscoveryEvent::Updated(p)) =
                browser.recv_timeout(Duration::from_millis(500))
                && p.id == id
            {
                found = Some(p);
                break;
            }
        }
        let p = found.expect("did not discover own advertisement; is multicast blocked?");
        assert_eq!(p.name, "Loopback Test");
        assert_eq!(p.port, 45_123);
        assert_eq!(p.version, PROTO_VERSION);
        assert!(!p.addrs.is_empty());

        drop(adv);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut lost = false;
        while std::time::Instant::now() < deadline {
            if let Some(DiscoveryEvent::Lost(l)) = browser.recv_timeout(Duration::from_millis(500))
                && l == id
            {
                lost = true;
                break;
            }
        }
        assert!(lost, "goodbye on drop was not observed");
    }
}
