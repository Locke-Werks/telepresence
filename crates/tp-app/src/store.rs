//! On-disk state: this install's identity and the peers it has pinned.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tp_proto::{Fingerprint, Identity, PeerStore};

/// `%APPDATA%\TelePresence` on Windows, the XDG config dir elsewhere.
/// `TELEPRESENCE_CONFIG_DIR` overrides it, which is how tests and a second
/// instance on one machine get a separate identity.
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TELEPRESENCE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("TelePresence")
}

/// Sender and receiver keep separate identities and pin files: a machine
/// that runs both roles must not treat a peer paired in one role as trusted
/// in the other.
pub fn role_dir(base: &Path, role: &str) -> PathBuf {
    base.join(role)
}

pub fn load_identity(dir: &Path) -> anyhow::Result<Identity> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("identity.pem");
    Identity::load_or_generate(&path).with_context(|| format!("loading identity {}", path.display()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PinnedPeer {
    pub fingerprint: String,
    pub name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PeerFile {
    peers: Vec<PinnedPeer>,
}

/// Pinned peers as JSON. The file is the only copy: every query reads it and
/// every change is a read-modify-write under a lock file. More than one
/// instance opens the same file (the running endpoint, the GUI's paired list
/// and Forget button, a CLI run beside the GUI), and an in-memory snapshot
/// written back whole would drop whatever another instance pinned since.
#[derive(Debug)]
pub struct JsonPeerStore {
    path: PathBuf,
    /// Serializes writers inside this process; the lock file covers the rest.
    write: Mutex<()>,
}

impl JsonPeerStore {
    /// Fails if the file exists but cannot be read or parsed, so a corrupt
    /// file is reported instead of being treated as "no pins" and overwritten.
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let store = Self { path: path.into(), write: Mutex::new(()) };
        store.load()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> anyhow::Result<BTreeMap<Fingerprint, String>> {
        let mut peers = BTreeMap::new();
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let file: PeerFile =
                    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", self.path.display()))?;
                for p in file.peers {
                    match Fingerprint::from_hex(&p.fingerprint) {
                        Some(fp) => {
                            peers.insert(fp, p.name);
                        }
                        None => tracing::warn!("ignoring malformed fingerprint in {}", self.path.display()),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
        Ok(peers)
    }

    /// Reads the current file, applies `f`, and writes the result if `f`
    /// says it changed something. A read failure aborts rather than writing
    /// over pins it could not see.
    fn update<R>(&self, f: impl FnOnce(&mut BTreeMap<Fingerprint, String>) -> (bool, R)) -> anyhow::Result<R> {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let lock_path = self.path.with_extension("json.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        lock.lock().with_context(|| format!("locking {}", lock_path.display()))?;
        let mut peers = self.load()?;
        let (changed, out) = f(&mut peers);
        if changed {
            self.save(&peers).with_context(|| format!("saving {}", self.path.display()))?;
        }
        Ok(out)
    }

    pub fn list(&self) -> Vec<PinnedPeer> {
        self.load()
            .unwrap_or_else(|e| {
                tracing::warn!("{e:#}");
                BTreeMap::new()
            })
            .iter()
            .map(|(fp, name)| PinnedPeer { fingerprint: fp.to_hex(), name: name.clone() })
            .collect()
    }

    /// Drops a pin by hex fingerprint. Returns whether one was removed. A
    /// running endpoint sees the change at its next handshake.
    pub fn forget(&self, fingerprint_hex: &str) -> anyhow::Result<bool> {
        let Some(fp) = Fingerprint::from_hex(fingerprint_hex) else { return Ok(false) };
        let removed = self.update(|peers| {
            let r = peers.remove(&fp).is_some();
            (r, r)
        })?;
        if removed {
            tracing::info!("forgot peer {fp} in {}", self.path.display());
        }
        Ok(removed)
    }

    pub fn is_empty(&self) -> bool {
        self.load().map(|p| p.is_empty()).unwrap_or(true)
    }

    fn save(&self, peers: &BTreeMap<Fingerprint, String>) -> std::io::Result<()> {
        let file = PeerFile {
            peers: peers.iter().map(|(fp, n)| PinnedPeer { fingerprint: fp.to_hex(), name: n.clone() }).collect(),
        };
        // Write then rename so a crash mid-write cannot wipe every pin.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&file).expect("serializable"))?;
        std::fs::rename(&tmp, &self.path)
    }
}

impl PeerStore for JsonPeerStore {
    fn is_pinned(&self, fingerprint: &Fingerprint) -> bool {
        match self.load() {
            Ok(p) => p.contains_key(fingerprint),
            Err(e) => {
                tracing::error!("{e:#}");
                false
            }
        }
    }

    fn pin(&self, fingerprint: Fingerprint, name: &str) {
        let res = self.update(|peers| {
            let changed = peers.get(&fingerprint).map(String::as_str) != Some(name);
            peers.insert(fingerprint, name.to_string());
            (changed, ())
        });
        match res {
            Ok(()) => tracing::info!("pinned peer {name} ({fingerprint}) in {}", self.path.display()),
            Err(e) => tracing::error!("could not save pinned peer {name} ({fingerprint}): {e:#}"),
        }
    }

    fn unpin(&self, fingerprint: &Fingerprint) {
        let res = self.update(|peers| {
            let r = peers.remove(fingerprint).is_some();
            (r, r)
        });
        match res {
            Ok(true) => tracing::info!(
                "peer {fingerprint} no longer recognises this install; dropped its stale pin from {}",
                self.path.display()
            ),
            Ok(false) => {}
            Err(e) => tracing::error!("could not drop stale pin {fingerprint}: {e:#}"),
        }
    }
}

/// Devices the sender is pinned to, by id. Kept so a run without flags uses
/// the same hardware as the last one instead of whatever the OS calls default
/// at the moment (which moves to AirPods when they connect).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SenderSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic: Option<String>,
    /// Forced capture size; absent means the camera's native mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Manual bitrate; absent means automatic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    /// Last receiver a session was established with, as `ip:port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_peer: Option<String>,
}

/// Receiver choices the GUI remembers. The CLI takes these as flags.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiverSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// "auto", "default", "none", or an output name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_out: Option<String>,
}

/// Which role the GUI opens in.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

fn load_json<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn save_json<T: Serialize>(path: &Path, v: &T) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Write then rename so a crash mid-write cannot leave a truncated file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v).expect("serializable"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

impl ReceiverSettings {
    pub fn load(receiver_dir: &Path) -> anyhow::Result<Self> {
        load_json(&receiver_dir.join("settings.json"))
    }
    pub fn save(&self, receiver_dir: &Path) -> anyhow::Result<()> {
        save_json(&receiver_dir.join("settings.json"), self)
    }
}

impl AppSettings {
    pub fn load(base: &Path) -> anyhow::Result<Self> {
        load_json(&base.join("settings.json"))
    }
    pub fn save(&self, base: &Path) -> anyhow::Result<()> {
        save_json(&base.join("settings.json"), self)
    }
}

impl SenderSettings {
    pub fn path(sender_dir: &Path) -> PathBuf {
        sender_dir.join("settings.json")
    }

    pub fn load(sender_dir: &Path) -> anyhow::Result<Self> {
        load_json(&Self::path(sender_dir))
    }

    pub fn save(&self, sender_dir: &Path) -> anyhow::Result<()> {
        save_json(&Self::path(sender_dir), self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let dir = std::env::temp_dir().join(format!("tp-app-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(SenderSettings::load(&dir).unwrap(), SenderSettings::default());
        let s = SenderSettings {
            camera: Some("cam-1".into()),
            mic: Some("BuiltInMicrophoneDevice".into()),
            size: Some((1920, 1080)),
            ..Default::default()
        };
        s.save(&dir).unwrap();
        assert_eq!(SenderSettings::load(&dir).unwrap(), s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pins_survive_reopen() {
        let dir = std::env::temp_dir().join(format!("tp-app-store-{}", std::process::id()));
        let path = dir.join("peers.json");
        let _ = std::fs::remove_dir_all(&dir);
        let fp = Fingerprint([7; 32]);
        {
            let s = JsonPeerStore::open(&path).unwrap();
            assert!(!s.is_pinned(&fp));
            s.pin(fp, "laptop");
        }
        let s = JsonPeerStore::open(&path).unwrap();
        assert!(s.is_pinned(&fp));
        assert_eq!(s.list()[0].name, "laptop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two stores on one file, as when the GUI lists or forgets pins while the
    /// running receiver pins a new sender. Neither may drop the other's work.
    #[test]
    fn concurrent_instances_do_not_clobber() {
        let dir = std::env::temp_dir().join(format!("tp-app-clobber-{}", std::process::id()));
        let path = dir.join("peers.json");
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b, c) = (Fingerprint([1; 32]), Fingerprint([2; 32]), Fingerprint([3; 32]));
        // Both open before either pins, like a receiver started before a CLI run.
        let running = JsonPeerStore::open(&path).unwrap();
        let other = JsonPeerStore::open(&path).unwrap();
        other.pin(a, "mac");
        running.pin(b, "laptop");
        assert!(running.is_pinned(&a), "a pin made by another instance must be visible");
        assert!(other.is_pinned(&b));
        // A forget through one instance holds for the other and is not undone
        // by that instance's next pin.
        assert!(other.forget(&a.to_hex()).unwrap());
        assert!(!running.is_pinned(&a));
        running.pin(c, "desk");
        let mut names: Vec<_> = JsonPeerStore::open(&path).unwrap().list().into_iter().map(|p| p.name).collect();
        names.sort();
        assert_eq!(names, ["desk", "laptop"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("tp-app-corrupt-{}", std::process::id()));
        let path = dir.join("peers.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(JsonPeerStore::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
