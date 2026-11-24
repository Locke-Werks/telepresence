//! In-app updates from GitHub Releases: check, download, verify, apply.
//!
//! Nothing here touches Qt, so the whole flow is testable against a local
//! HTTP server. The bridge owns the UI state and calls into this module.
//!
//! `TELEPRESENCE_UPDATE_URL` replaces the releases/latest endpoint. It is a
//! development switch for pointing a build at a mock server, and because a
//! mock is plain HTTP it also lifts the HTTPS requirement. Never set it on a
//! machine that should receive real updates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const RELEASES_LATEST: &str = "https://api.github.com/repos/Locke-Werks/telepresence/releases/latest";
pub const URL_ENV: &str = "TELEPRESENCE_UPDATE_URL";
pub const USER_AGENT: &str = "TelePresence-Updater";
pub const SUMS_NAME: &str = "SHA256SUMS";
/// Automatic checks run at most this often. Manual checks ignore it.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

const SUMS_LIMIT: u64 = 1 << 20;
const ASSET_LIMIT: u64 = 4 << 30;

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is semver")
}

// ---------------------------------------------------------------- versions

/// "v1.2.3" or "1.2.3". Anything else is not a release this updater offers.
pub fn parse_tag(tag: &str) -> Option<Version> {
    let t = tag.trim();
    Version::parse(t.strip_prefix('v').unwrap_or(t)).ok()
}

/// A prerelease never counts as newer: GitHub's `prerelease` flag is set by
/// hand, and a tag like 1.0.0-rc1 marked as a full release by mistake must
/// not reach everyone.
pub fn is_newer(current: &Version, candidate: &Version) -> bool {
    candidate.pre.is_empty() && candidate > current
}

// --------------------------------------------------------------- checksums

/// Parses `sha256sum` output: `<64 hex>  <name>`, or `<hex> *<name>` for
/// binary mode. Names keep any directory prefix stripped, since CI may hash
/// from inside a dist folder. Malformed lines are skipped rather than failing
/// the file, because only the one asset being installed has to be present.
pub fn parse_sha256sums(text: &str) -> HashMap<String, [u8; 32]> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some((hex, rest)) = line.split_once(char::is_whitespace) else { continue };
        let name = rest.trim_start().trim_start_matches('*').trim();
        let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if name.is_empty() {
            continue;
        }
        if let Some(d) = decode_hex32(hex) {
            out.insert(name.to_string(), d);
        }
    }
    out
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| (c as char).to_digit(16).map(|v| v as u8);
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks(2).enumerate() {
        out[i] = nib(pair[0])? << 4 | nib(pair[1])?;
    }
    Some(out)
}

pub fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------ release JSON

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub html_url: String,
    pub assets: Vec<Asset>,
}

impl Release {
    /// `None` for drafts, prereleases and tags that are not semver.
    pub fn from_json(body: &str) -> anyhow::Result<Option<Self>> {
        let r: GhRelease = serde_json::from_str(body).context("parsing release JSON")?;
        if r.draft || r.prerelease {
            return Ok(None);
        }
        let Some(version) = parse_tag(&r.tag_name) else { return Ok(None) };
        Ok(Some(Self { version, tag: r.tag_name, html_url: r.html_url, assets: r.assets }))
    }

    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

// ------------------------------------------------------ install and assets

/// How this copy was installed, which decides what an update can do to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// Running from a cargo target dir. Updates are shown, never applied.
    Dev,
    /// Installed by the Forge setup exe.
    Windows,
    /// Inside an .app bundle at this path.
    MacBundle(PathBuf),
    /// An AppImage at this path (`$APPIMAGE`).
    AppImage(PathBuf),
    /// Installed from the .deb: the package manager owns the files.
    Deb,
    /// Something else, such as a bare binary copied by hand. Shown, not applied.
    Unknown,
}

impl InstallKind {
    pub fn can_apply(&self) -> bool {
        matches!(self, Self::Windows | Self::MacBundle(_) | Self::AppImage(_))
    }
}

pub fn detect_install() -> InstallKind {
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).unwrap_or_default();
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    classify(&exe, std::env::consts::OS, appimage)
}

/// The decision behind [`detect_install`], with its inputs passed in.
pub fn classify(exe: &Path, os: &str, appimage: Option<PathBuf>) -> InstallKind {
    if is_dev_path(exe) {
        return InstallKind::Dev;
    }
    match os {
        "windows" => InstallKind::Windows,
        "macos" => exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .map(|p| InstallKind::MacBundle(p.to_path_buf()))
            .unwrap_or(InstallKind::Unknown),
        "linux" => match appimage {
            Some(p) if !p.as_os_str().is_empty() => InstallKind::AppImage(p),
            _ if exe.starts_with("/usr") => InstallKind::Deb,
            _ => InstallKind::Unknown,
        },
        _ => InstallKind::Unknown,
    }
}

/// A `target` directory beside a Cargo.toml is a build tree. Requiring the
/// manifest keeps an install path that merely contains "target" from being
/// mistaken for one.
fn is_dev_path(exe: &Path) -> bool {
    exe.ancestors().any(|p| {
        p.file_name().is_some_and(|n| n == "target") && p.parent().is_some_and(|d| d.join("Cargo.toml").is_file())
    })
}

/// What the update for this install is downloaded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Package {
    WindowsSetup,
    MacZip,
    MacDmg,
    AppImage,
    Deb,
}

/// The release asset name CI publishes for `pkg` at `version`, or `None` when
/// no build exists for this architecture.
pub fn asset_name(pkg: Package, version: &Version, arch: &str) -> Option<String> {
    let v = version;
    Some(match (pkg, arch) {
        (Package::WindowsSetup, "x86_64") => format!("telepresence-{v}-windows-x64-setup.exe"),
        (Package::MacZip, "aarch64") => format!("telepresence-{v}-macos-arm64.zip"),
        (Package::MacDmg, "aarch64") => format!("telepresence-{v}-macos-arm64.dmg"),
        (Package::AppImage, "x86_64") => format!("telepresence-{v}-linux-x86_64.AppImage"),
        (Package::Deb, "x86_64") => format!("telepresence_{v}_amd64.deb"),
        _ => return None,
    })
}

/// Which package an install takes. `bundle_writable` only matters on macOS:
/// a bundle the user cannot replace gets the disk image to drag in instead.
pub fn package_for(kind: &InstallKind, bundle_writable: bool) -> Option<Package> {
    match kind {
        InstallKind::Windows => Some(Package::WindowsSetup),
        InstallKind::MacBundle(_) if bundle_writable => Some(Package::MacZip),
        InstallKind::MacBundle(_) => Some(Package::MacDmg),
        InstallKind::AppImage(_) => Some(Package::AppImage),
        InstallKind::Deb => Some(Package::Deb),
        InstallKind::Dev | InstallKind::Unknown => None,
    }
}

pub fn select_asset<'r>(release: &'r Release, pkg: Package, arch: &str) -> Option<&'r Asset> {
    release.asset(&asset_name(pkg, &release.version, arch)?)
}

// ------------------------------------------------------- defer for streams

/// Holds an install back while a stream is live. The UI asks before starting
/// the download and again once the file is ready, since a stream can begin
/// in between; when one ends with a request armed, the install proceeds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyGate {
    armed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    Proceed,
    Wait,
}

impl ApplyGate {
    pub fn request(&mut self, streaming: bool) -> GateAction {
        self.armed = streaming;
        if streaming { GateAction::Wait } else { GateAction::Proceed }
    }

    pub fn stream_changed(&mut self, streaming: bool) -> GateAction {
        if self.armed && !streaming {
            self.armed = false;
            GateAction::Proceed
        } else {
            GateAction::Wait
        }
    }

    pub fn cancel(&mut self) {
        self.armed = false;
    }

    pub fn armed(&self) -> bool {
        self.armed
    }
}

// ---------------------------------------------------------------- settings

/// `update.json` in the config dir. Kept apart from settings.json because the
/// session code writes that file whole.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateSettings {
    #[serde(default = "yes")]
    pub auto_check: bool,
    /// Unix seconds of the last check that reached GitHub.
    #[serde(default)]
    pub last_check: Option<u64>,
    #[serde(default)]
    pub etag: Option<String>,
    /// The body the ETag belongs to, replayed on a 304.
    #[serde(default)]
    pub cached: Option<String>,
    #[serde(default)]
    pub skipped: Option<String>,
}

fn yes() -> bool {
    true
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self { auto_check: true, last_check: None, etag: None, cached: None, skipped: None }
    }
}

impl UpdateSettings {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("update.json")
    }

    /// A missing or unreadable file is the defaults: losing the ETag or the
    /// skip choice costs one extra request, not a broken updater.
    pub fn load(dir: &Path) -> Self {
        std::fs::read(Self::path(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("update.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, Self::path(dir))?;
        Ok(())
    }

    pub fn due(&self, now: u64) -> bool {
        self.auto_check && self.last_check.is_none_or(|t| now.saturating_sub(t) >= CHECK_INTERVAL.as_secs())
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ------------------------------------------------------------------ client

#[derive(Debug)]
pub enum CheckResult {
    UpToDate,
    Available(Release),
    /// GitHub refused for the hour. `reset` is when it lifts, in Unix seconds.
    RateLimited { reset: Option<u64> },
}

pub struct Updater {
    client: reqwest::Client,
    url: String,
    allow_http: bool,
}

impl Updater {
    /// Uses `TELEPRESENCE_UPDATE_URL` when set, otherwise GitHub.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var(URL_ENV) {
            Ok(u) if !u.trim().is_empty() => {
                tracing::warn!("{URL_ENV} is set: checking {u} for updates (development only)");
                Self::new(u.trim(), true)
            }
            _ => Self::new(RELEASES_LATEST, false),
        }
    }

    pub fn new(url: &str, allow_http: bool) -> anyhow::Result<Self> {
        // reqwest is built without a crypto provider so that only ring is
        // linked, the same one tp-proto uses. Losing the race to install it
        // means another caller already did, which is the same outcome.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let redirect = reqwest::redirect::Policy::custom(move |a| {
            if a.previous().len() >= 10 {
                a.error("too many redirects")
            } else if !allow_http && a.url().scheme() != "https" {
                a.error("redirect away from HTTPS refused")
            } else {
                a.follow()
            }
        });
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .redirect(redirect)
            .build()
            .context("building HTTP client")?;
        let this = Self { client, url: url.to_string(), allow_http };
        this.check_scheme(url)?;
        Ok(this)
    }

    fn check_scheme(&self, url: &str) -> anyhow::Result<()> {
        if self.allow_http || url.starts_with("https://") {
            Ok(())
        } else {
            bail!("refusing non-HTTPS update URL {url}")
        }
    }

    /// One conditional request to releases/latest. Updates `settings` with the
    /// ETag, the cached body and the check time; the caller saves it.
    pub async fn check(&self, settings: &mut UpdateSettings, current: &Version) -> anyhow::Result<CheckResult> {
        let mut req = self.client.get(&self.url).header("Accept", "application/vnd.github+json");
        // Only send the ETag when the body it stands for is still on hand: a
        // 304 with nothing to replay would leave the check without an answer.
        if let (Some(tag), Some(_)) = (&settings.etag, &settings.cached) {
            req = req.header("If-None-Match", tag.as_str());
        }
        let resp = req.timeout(Duration::from_secs(30)).send().await.context("contacting the update server")?;
        let status = resp.status().as_u16();
        let header = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = match status {
            304 => {
                settings.last_check = Some(now_secs());
                settings.cached.clone().unwrap_or_default()
            }
            200 => {
                let etag = header("etag");
                let body = resp.text().await.context("reading release JSON")?;
                settings.etag = etag;
                settings.cached = Some(body.clone());
                settings.last_check = Some(now_secs());
                body
            }
            // No published release yet is not a failure.
            404 => {
                settings.last_check = Some(now_secs());
                return Ok(CheckResult::UpToDate);
            }
            403 | 429 if status == 429 || header("x-ratelimit-remaining").as_deref() == Some("0") => {
                // Counts as a check, so a rate-limited machine does not retry
                // on every launch and stay limited.
                settings.last_check = Some(now_secs());
                let reset = header("x-ratelimit-reset").and_then(|v| v.parse().ok());
                return Ok(CheckResult::RateLimited { reset });
            }
            s => bail!("update server answered HTTP {s}"),
        };
        Ok(match Release::from_json(&body)? {
            Some(r) if is_newer(current, &r.version) => CheckResult::Available(r),
            _ => CheckResult::UpToDate,
        })
    }

    async fn get(&self, url: &str) -> anyhow::Result<reqwest::Response> {
        self.check_scheme(url)?;
        let resp = self.client.get(url).header("Accept", "application/octet-stream").send().await?;
        let s = resp.status();
        if !s.is_success() {
            bail!("downloading {url}: HTTP {}", s.as_u16());
        }
        Ok(resp)
    }

    pub async fn fetch_sums(&self, release: &Release) -> anyhow::Result<HashMap<String, [u8; 32]>> {
        let a = release.asset(SUMS_NAME).ok_or_else(|| anyhow!("release {} has no {SUMS_NAME}", release.tag))?;
        let mut resp = self.get(&a.browser_download_url).await?;
        let mut buf = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            buf.extend_from_slice(&chunk);
            if buf.len() as u64 > SUMS_LIMIT {
                bail!("{SUMS_NAME} is implausibly large");
            }
        }
        Ok(parse_sha256sums(&String::from_utf8_lossy(&buf)))
    }

    /// Downloads `asset` into `dir` and returns its path only once the digest
    /// matches SHA256SUMS. Bytes land in a `.part` file first, so nothing ever
    /// sees an unverified file under the real name; a mismatch deletes it.
    pub async fn download(
        &self,
        release: &Release,
        asset: &Asset,
        dir: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> anyhow::Result<PathBuf> {
        let sums = self.fetch_sums(release).await?;
        let want = *sums.get(&asset.name).ok_or_else(|| anyhow!("{SUMS_NAME} does not list {}", asset.name))?;
        if asset.name.contains(['/', '\\']) || asset.name.starts_with('.') {
            bail!("unexpected asset name {}", asset.name);
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let dest = dir.join(&asset.name);
        let part = dir.join(format!("{}.part", asset.name));
        let mut resp = self.get(&asset.browser_download_url).await?;
        let total = resp.content_length().unwrap_or(asset.size);
        let res: anyhow::Result<()> = async {
            use std::io::Write;
            let mut f = std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
            let mut hasher = Sha256::new();
            let mut got = 0u64;
            while let Some(chunk) = resp.chunk().await? {
                got += chunk.len() as u64;
                if got > ASSET_LIMIT || (asset.size > 0 && got > asset.size) {
                    bail!("{} is larger than the release says", asset.name);
                }
                hasher.update(&chunk);
                f.write_all(&chunk)?;
                progress(got, total);
            }
            f.sync_all()?;
            let have: [u8; 32] = hasher.finalize().into();
            if have != want {
                bail!("checksum mismatch for {}: expected {}, got {}", asset.name, hex(&want), hex(&have));
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        let _ = std::fs::remove_file(&dest);
        std::fs::rename(&part, &dest)?;
        Ok(dest)
    }
}

/// Per-version download directory under the system temp dir.
pub fn download_dir(version: &Version) -> PathBuf {
    std::env::temp_dir().join("TelePresence-update").join(version.to_string())
}

// ------------------------------------------------------------------- apply

/// What the app does after a successful apply.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// The new version is on its way in; quit now.
    Exit,
    /// Something was opened for the user to finish (the disk image).
    Handoff(String),
}

/// Applies a verified download. Only call with a path [`Updater::download`]
/// returned for this install's package.
pub fn apply(kind: &InstallKind, file: &Path) -> anyhow::Result<Applied> {
    match kind {
        #[cfg(windows)]
        InstallKind::Windows => windows::apply(file),
        #[cfg(target_os = "macos")]
        InstallKind::MacBundle(bundle) => macos::apply(bundle, file),
        #[cfg(target_os = "linux")]
        InstallKind::AppImage(path) => linux::apply(path, file),
        _ => bail!("this copy of TelePresence cannot update itself"),
    }
}

/// Whether the directory holding an .app bundle accepts a new sibling, which
/// is what replacing the bundle takes.
pub fn bundle_writable(bundle: &Path) -> bool {
    let Some(parent) = bundle.parent() else { return false };
    let probe = parent.join(format!(".tp-write-probe-{}", std::process::id()));
    let ok = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    /// Forge cannot relaunch the product after a silent install, so a hidden
    /// PowerShell waits on the installer and starts the app again. It runs
    /// with this process's unelevated token: `Start-Process` goes through
    /// ShellExecute, which raises UAC for the installer's manifest, and the
    /// relaunch inherits the waiter's token rather than the installer's.
    /// The relaunch runs even if the install was declined or failed, since a
    /// failed Forge install rolls back to the copy that is already there.
    /// Paths go through the environment so no quoting is involved.
    pub fn apply(setup: &Path) -> anyhow::Result<Applied> {
        let exe = std::env::current_exe()?;
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let ps = root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let script = "try { Start-Process -FilePath $env:TP_UPDATE_SETUP -ArgumentList '/S' -Wait } catch {}; \
                      Start-Process -FilePath $env:TP_UPDATE_EXE";
        std::process::Command::new(ps)
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command", script])
            .env("TP_UPDATE_SETUP", setup)
            .env("TP_UPDATE_EXE", &exe)
            .current_dir(std::env::temp_dir())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .context("starting the installer")?;
        Ok(Applied::Exit)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::process::Command;

    fn run(cmd: &mut Command, what: &str) -> anyhow::Result<()> {
        let out = cmd.output().with_context(|| format!("running {what}"))?;
        if !out.status.success() {
            bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    pub fn apply(bundle: &Path, file: &Path) -> anyhow::Result<Applied> {
        if file.extension().is_some_and(|e| e == "dmg") {
            run(Command::new("/usr/bin/open").arg(file), "open")?;
            return Ok(Applied::Handoff(format!(
                "Drag TelePresence from the disk image into {} to finish updating.",
                bundle.parent().map(|p| p.display().to_string()).unwrap_or_default()
            )));
        }
        let work = file.with_extension("extract");
        let _ = std::fs::remove_dir_all(&work);
        // ditto keeps the symlinks and extended attributes a signed bundle
        // depends on; a generic unzip can break the signature.
        run(Command::new("/usr/bin/ditto").arg("-x").arg("-k").arg(file).arg(&work), "ditto")?;
        let new_app = std::fs::read_dir(&work)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or_else(|| anyhow!("no .app in the update archive"))?;
        run(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(&new_app), "codesign")?;
        run(Command::new("/usr/sbin/spctl").args(["-a", "-t", "exec"]).arg(&new_app), "spctl")?;

        // Stage next to the bundle so the swap is two renames on one volume.
        let parent = bundle.parent().ok_or_else(|| anyhow!("bundle has no parent"))?;
        let name = bundle.file_name().ok_or_else(|| anyhow!("bundle has no name"))?.to_string_lossy().to_string();
        let staged = parent.join(format!(".{name}.new"));
        let old = parent.join(format!("{name}.old"));
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&old);
        run(Command::new("/usr/bin/ditto").arg(&new_app).arg(&staged), "ditto")?;
        std::fs::rename(bundle, &old).context("moving the old bundle aside")?;
        if let Err(e) = std::fs::rename(&staged, bundle) {
            let _ = std::fs::rename(&old, bundle);
            return Err(e).context("moving the new bundle in");
        }
        // The running process keeps its mapped files; removing the old bundle
        // under it is safe on macOS.
        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::remove_dir_all(&work);
        Command::new("/usr/bin/open").arg("-n").arg(bundle).spawn().context("relaunching")?;
        Ok(Applied::Exit)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub fn apply(appimage: &Path, file: &Path) -> anyhow::Result<Applied> {
        let dir = appimage.parent().ok_or_else(|| anyhow!("AppImage has no parent dir"))?;
        let staged = dir.join(format!(
            ".{}.new",
            appimage.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
        ));
        // Copy into the AppImage's own directory first: /tmp is often another
        // filesystem, and only a same-volume rename replaces the file atomically.
        std::fs::copy(file, &staged).with_context(|| format!("writing {}", staged.display()))?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        if let Err(e) = std::fs::rename(&staged, appimage) {
            let _ = std::fs::remove_file(&staged);
            return Err(e).with_context(|| format!("replacing {}", appimage.display()));
        }
        std::process::Command::new(appimage).spawn().context("relaunching")?;
        Ok(Applied::Exit)
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn version_compare() {
        assert_eq!(parse_tag("v1.2.3"), Some(v("1.2.3")));
        assert_eq!(parse_tag("1.2.3"), Some(v("1.2.3")));
        assert_eq!(parse_tag("release-1"), None);
        assert!(is_newer(&v("0.1.0"), &v("0.1.1")));
        assert!(is_newer(&v("0.1.9"), &v("0.2.0")));
        assert!(is_newer(&v("0.9.0"), &v("0.10.0")), "numeric, not lexical");
        assert!(!is_newer(&v("0.2.0"), &v("0.2.0")));
        assert!(!is_newer(&v("0.2.0"), &v("0.1.5")));
        assert!(!is_newer(&v("0.1.0"), &v("0.2.0-rc.1")), "prereleases never offered");
    }

    #[test]
    fn sums_parsing() {
        let a = "a".repeat(64);
        let b = "0123456789abcdef".repeat(4);
        let text = format!(
            "{a}  telepresence-1.0.0-windows-x64-setup.exe\r\n{b} *dist/telepresence_1.0.0_amd64.deb\nnot a line\n{}  short\n\n",
            "ab"
        );
        let m = parse_sha256sums(&text);
        assert_eq!(m.len(), 2);
        assert_eq!(m["telepresence-1.0.0-windows-x64-setup.exe"], [0xaa; 32]);
        assert_eq!(hex(&m["telepresence_1.0.0_amd64.deb"]), b);
        assert!(decode_hex32(&"zz".repeat(32)).is_none());
    }

    fn release(ver: &str, names: &[&str]) -> Release {
        Release {
            version: v(ver),
            tag: format!("v{ver}"),
            html_url: String::new(),
            assets: names
                .iter()
                .map(|n| Asset { name: n.to_string(), browser_download_url: format!("https://x/{n}"), size: 1 })
                .collect(),
        }
    }

    #[test]
    fn asset_selection() {
        let r = release(
            "1.4.0",
            &[
                "telepresence-1.4.0-windows-x64-setup.exe",
                "telepresence-1.4.0-macos-arm64.zip",
                "telepresence-1.4.0-macos-arm64.dmg",
                "telepresence-1.4.0-linux-x86_64.AppImage",
                "telepresence_1.4.0_amd64.deb",
                "SHA256SUMS",
            ],
        );
        let pick = |k: &InstallKind, w: bool, arch: &str| {
            package_for(k, w).and_then(|p| select_asset(&r, p, arch)).map(|a| a.name.as_str())
        };
        let mac = InstallKind::MacBundle("/Applications/TelePresence.app".into());
        assert_eq!(pick(&InstallKind::Windows, true, "x86_64"), Some("telepresence-1.4.0-windows-x64-setup.exe"));
        assert_eq!(pick(&InstallKind::Windows, true, "aarch64"), None, "no Windows arm64 build");
        assert_eq!(pick(&mac, true, "aarch64"), Some("telepresence-1.4.0-macos-arm64.zip"));
        assert_eq!(pick(&mac, false, "aarch64"), Some("telepresence-1.4.0-macos-arm64.dmg"));
        assert_eq!(pick(&mac, true, "x86_64"), None, "no Intel Mac build");
        let ai = InstallKind::AppImage("/home/u/TelePresence.AppImage".into());
        assert_eq!(pick(&ai, true, "x86_64"), Some("telepresence-1.4.0-linux-x86_64.AppImage"));
        assert_eq!(pick(&InstallKind::Deb, true, "x86_64"), Some("telepresence_1.4.0_amd64.deb"));
        assert_eq!(pick(&InstallKind::Dev, true, "x86_64"), None);
        assert!(!InstallKind::Deb.can_apply() && !InstallKind::Dev.can_apply());
    }

    #[test]
    fn install_classification() {
        let tmp = std::env::temp_dir().join(format!("tp-classify-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("target/debug")).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        assert_eq!(classify(&tmp.join("target/debug/telepresence.exe"), "windows", None), InstallKind::Dev);
        // "target" without a manifest beside it is an ordinary directory.
        let installed = Path::new("/opt/target/telepresence");
        assert_eq!(classify(installed, "windows", None), InstallKind::Windows);
        assert_eq!(
            classify(Path::new("/Applications/TelePresence.app/Contents/MacOS/telepresence"), "macos", None),
            InstallKind::MacBundle("/Applications/TelePresence.app".into())
        );
        assert_eq!(
            classify(Path::new("/tmp/.mount_x/usr/bin/telepresence"), "linux", Some("/home/u/t.AppImage".into())),
            InstallKind::AppImage("/home/u/t.AppImage".into())
        );
        assert_eq!(classify(Path::new("/usr/bin/telepresence"), "linux", None), InstallKind::Deb);
        assert_eq!(classify(Path::new("/home/u/bin/telepresence"), "linux", None), InstallKind::Unknown);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn defer_during_stream() {
        let mut g = ApplyGate::default();
        // Idle: go straight ahead.
        assert_eq!(g.request(false), GateAction::Proceed);
        assert!(!g.armed());
        // Streaming: wait, and nothing happens while the stream continues.
        assert_eq!(g.request(true), GateAction::Wait);
        assert!(g.armed());
        assert_eq!(g.stream_changed(true), GateAction::Wait);
        // The stream ends: proceed exactly once.
        assert_eq!(g.stream_changed(false), GateAction::Proceed);
        assert_eq!(g.stream_changed(false), GateAction::Wait);
        // Without a request, a stream ending does nothing.
        assert_eq!(g.stream_changed(false), GateAction::Wait);
        // Cancelled (Later) while armed: the end of the stream is ignored.
        g.request(true);
        g.cancel();
        assert_eq!(g.stream_changed(false), GateAction::Wait);
    }

    #[test]
    fn settings_due() {
        let mut s = UpdateSettings::default();
        assert!(s.auto_check && s.due(1_000_000));
        s.last_check = Some(1_000_000);
        assert!(!s.due(1_000_000 + 3600));
        assert!(s.due(1_000_000 + CHECK_INTERVAL.as_secs()));
        s.auto_check = false;
        assert!(!s.due(u64::MAX));
        let parsed: UpdateSettings = serde_json::from_str("{}").unwrap();
        assert!(parsed.auto_check, "a missing key means automatic checks stay on");
    }

    // ---------------------------------------------------- local mock server

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    struct Mock {
        base: String,
        /// Request lines plus If-None-Match, for asserting what was sent.
        log: Arc<Mutex<Vec<String>>>,
    }

    /// A tiny HTTP/1.1 server: `routes` maps a path to (status, headers, body).
    /// The release JSON may hold `{base}`, replaced with the server's own URL.
    type Route = (&'static str, u16, Vec<(&'static str, String)>, Vec<u8>);

    fn mock(routes: Vec<Route>) -> Mock {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let routes: Vec<_> = routes
            .into_iter()
            .map(|(p, s, h, b)| {
                let b = String::from_utf8(b.clone()).map(|t| t.replace("{base}", &base).into_bytes()).unwrap_or(b);
                (p, s, h, b)
            })
            .collect();
        let log2 = log.clone();
        std::thread::spawn(move || {
            for stream in l.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut inm = String::new();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    if h.to_ascii_lowercase().starts_with("if-none-match:") {
                        inm = h[14..].trim().to_string();
                    }
                }
                log2.lock().unwrap().push(format!("{path} {inm}"));
                let (status, headers, body) = match routes.iter().find(|(p, ..)| *p == path) {
                    Some((_, st, h, _)) if *st == 200 && !inm.is_empty() && h.iter().any(|(k, v)| *k == "ETag" && *v == inm) => {
                        (304, vec![], vec![])
                    }
                    Some((_, st, h, b)) => (*st, h.clone(), b.clone()),
                    None => (404, vec![], b"not found".to_vec()),
                };
                let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (k, v) in headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
                let _ = s.flush();
            }
        });
        Mock { base, log }
    }

    fn sha(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    fn release_json(ver: &str, asset: &str, size: usize) -> Vec<u8> {
        serde_json::json!({
            "tag_name": format!("v{ver}"),
            "draft": false,
            "prerelease": false,
            "html_url": "{base}/page",
            "assets": [
                {"name": asset, "browser_download_url": format!("{{base}}/dl/{asset}"), "size": size},
                {"name": "SHA256SUMS", "browser_download_url": "{base}/dl/SHA256SUMS", "size": 0},
            ]
        })
        .to_string()
        .into_bytes()
    }

    /// End to end against the mock: detect, conditional re-check, download,
    /// verify, and refuse a file whose digest is wrong. No installer runs.
    #[tokio::test]
    async fn mock_end_to_end() {
        let good = b"pretend this is a setup exe".to_vec();
        let name = "telepresence-9.9.9-windows-x64-setup.exe";
        let bad_name = "telepresence-9.9.8-windows-x64-setup.exe";
        let sums = format!("{}  {name}\n{}  {bad_name}\n", sha(&good), sha(b"something else"));
        let m = mock(vec![
            ("/latest", 200, vec![("ETag", "\"abc\"".into())], release_json("9.9.9", name, good.len())),
            ("/bad/latest", 200, vec![], release_json("9.9.8", bad_name, good.len())),
            ("/limited", 403, vec![("x-ratelimit-remaining", "0".into()), ("x-ratelimit-reset", "1700000000".into())], vec![]),
            ("/draft", 200, vec![], br#"{"tag_name":"v99.0.0","draft":true,"assets":[]}"#.to_vec()),
            ("/dl/SHA256SUMS", 200, vec![], sums.into_bytes()),
            (Box::leak(format!("/dl/{name}").into_boxed_str()), 200, vec![], good.clone()),
            (Box::leak(format!("/dl/{bad_name}").into_boxed_str()), 200, vec![], good.clone()),
        ]);
        let cur = v("0.1.0");
        let dir = std::env::temp_dir().join(format!("tp-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Detect.
        let up = Updater::new(&format!("{}/latest", m.base), true).unwrap();
        let mut st = UpdateSettings::default();
        let CheckResult::Available(rel) = up.check(&mut st, &cur).await.unwrap() else { panic!("expected update") };
        assert_eq!(rel.version, v("9.9.9"));
        assert_eq!(st.etag.as_deref(), Some("\"abc\""));
        assert!(st.last_check.is_some());

        // Second check sends the ETag, gets a 304 and still answers.
        let CheckResult::Available(again) = up.check(&mut st, &cur).await.unwrap() else { panic!("304 replay") };
        assert_eq!(again.version, rel.version);
        assert!(m.log.lock().unwrap().iter().any(|l| l == "/latest \"abc\""));

        // Same version is up to date.
        assert!(matches!(up.check(&mut st, &v("9.9.9")).await.unwrap(), CheckResult::UpToDate));

        // Download and verify.
        let asset = select_asset(&rel, Package::WindowsSetup, "x86_64").unwrap().clone();
        let mut seen = 0;
        let path = up.download(&rel, &asset, &dir, |got, _| seen = got).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), good);
        assert_eq!(seen, good.len() as u64);

        // Mismatch: rejected, and nothing left under either name.
        let bad_up = Updater::new(&format!("{}/bad/latest", m.base), true).unwrap();
        let CheckResult::Available(bad_rel) = bad_up.check(&mut UpdateSettings::default(), &cur).await.unwrap() else {
            panic!()
        };
        let bad_asset = select_asset(&bad_rel, Package::WindowsSetup, "x86_64").unwrap().clone();
        let err = bad_up.download(&bad_rel, &bad_asset, &dir, |_, _| {}).await.unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert!(!dir.join(bad_name).exists() && !dir.join(format!("{bad_name}.part")).exists());

        // Rate limit is an answer, not an error.
        let lim = Updater::new(&format!("{}/limited", m.base), true).unwrap();
        assert!(matches!(
            lim.check(&mut UpdateSettings::default(), &cur).await.unwrap(),
            CheckResult::RateLimited { reset: Some(1_700_000_000) }
        ));

        // Drafts are ignored.
        let dr = Updater::new(&format!("{}/draft", m.base), true).unwrap();
        assert!(matches!(dr.check(&mut UpdateSettings::default(), &cur).await.unwrap(), CheckResult::UpToDate));

        // Plain HTTP is refused unless the dev override allowed it.
        assert!(Updater::new(&format!("{}/latest", m.base), false).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
