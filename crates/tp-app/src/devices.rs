//! Which camera and microphone a real sender captures from.
//!
//! The system default input is never used: on a Mac it follows whatever audio
//! device connected last, so AirPods coming into range would silently move
//! the call's microphone. The sender is pinned to a device id instead, taken
//! from `--camera`/`--mic`, then from the saved settings, then picked once and
//! saved.

use std::path::Path;

use anyhow::bail;
use tp_media::{DeviceInfo, DeviceKind};

use crate::store::SenderSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Flag,
    Saved,
    Auto,
}

impl Origin {
    fn describe(self) -> &'static str {
        match self {
            Origin::Flag => "from the command line",
            Origin::Saved => "saved",
            Origin::Auto => "picked automatically",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Chosen {
    pub name: String,
    /// The device id, or the name for a provider that reports none. This is
    /// what gets saved and what the capture pipeline is handed.
    pub key: String,
    pub origin: Origin,
}

#[derive(Debug, Clone)]
pub struct DeviceChoice {
    pub camera: Chosen,
    pub mic: Chosen,
}

/// Resolves both devices. With `persist`, a choice that differs from the
/// saved one is written back so the next run reuses it.
pub fn resolve(
    sender_dir: &Path,
    camera: Option<&str>,
    mic: Option<&str>,
    persist: bool,
) -> anyhow::Result<DeviceChoice> {
    if mic.is_some_and(|m| m.trim().eq_ignore_ascii_case("default")) {
        bail!(
            "--mic default is not allowed: the system default input follows whichever device connected \
             last (AirPods, for one), so the call's microphone would move under you. Pass a device id or \
             name from `telepresence devices`."
        );
    }
    let mut settings = SenderSettings::load(sender_dir)?;
    let all = tp_media::list_devices()?;
    let camera = choose(&all, DeviceKind::Camera, camera, settings.camera.as_deref())?;
    let mic = choose(&all, DeviceKind::Microphone, mic, settings.mic.as_deref())?;

    if persist {
        let before = settings.clone();
        settings.camera = Some(camera.key.clone());
        settings.mic = Some(mic.key.clone());
        if settings != before {
            settings.save(sender_dir)?;
            tracing::info!("saved device choice to {}", SenderSettings::path(sender_dir).display());
        }
    }
    Ok(DeviceChoice { camera, mic })
}

pub fn log_choice(c: &DeviceChoice) {
    tracing::info!(
        "camera: {} [{}] ({}); change with --camera <id|name>",
        c.camera.name,
        c.camera.key,
        c.camera.origin.describe()
    );
    tracing::info!(
        "microphone: {} [{}] ({}); change with --mic <id|name>, ids from `telepresence devices`",
        c.mic.name,
        c.mic.key,
        c.mic.origin.describe()
    );
}

/// Native modes of the camera with this id or name, best first. Empty when
/// its caps cannot be read, in which case only "native" is meaningful.
pub fn camera_modes(key: &str) -> anyhow::Result<Vec<tp_media::CameraMode>> {
    let dev = tp_media::find_device(DeviceKind::Camera, key)?;
    let el = dev.create_element()?;
    Ok(tp_media::mode::device_caps(Some(&dev.device), &el)
        .map(|c| tp_media::mode::list_modes(&c))
        .unwrap_or_default())
}

/// Devices a picker may offer for `kind`: the pseudo "Default" entries are
/// left out because choosing one would defeat pinning.
pub fn pickable(all: &[DeviceInfo], kind: DeviceKind) -> Vec<(String, String)> {
    all.iter()
        .filter(|d| d.kind == kind && !(kind == DeviceKind::Microphone && is_pseudo_default(&d.name, d.id.as_deref())))
        .map(|d| (key_of(d), d.name.clone()))
        .collect()
}

fn label(kind: DeviceKind) -> (&'static str, &'static str) {
    match kind {
        DeviceKind::Camera => ("camera", "--camera"),
        DeviceKind::Microphone => ("microphone", "--mic"),
        DeviceKind::AudioOutput => ("audio output", "--audio-out"),
    }
}

fn key_of(d: &DeviceInfo) -> String {
    d.id.clone().unwrap_or_else(|| d.name.clone())
}

fn choose(all: &[DeviceInfo], kind: DeviceKind, flag: Option<&str>, saved: Option<&str>) -> anyhow::Result<Chosen> {
    let (what, opt) = label(kind);
    let of_kind: Vec<&DeviceInfo> = all.iter().filter(|d| d.kind == kind).collect();
    let listing = || {
        if of_kind.is_empty() {
            "none".to_string()
        } else {
            of_kind
                .iter()
                .map(|d| format!("\n  {} [{}]", d.name, d.id.as_deref().unwrap_or("no id")))
                .collect::<String>()
        }
    };
    if let Some(q) = flag.map(str::trim).filter(|q| !q.is_empty()) {
        return match find(&of_kind, q) {
            Some(d) => Ok(Chosen { name: d.name.clone(), key: key_of(d), origin: Origin::Flag }),
            None => bail!("no {what} matches \"{q}\". Available:{}", listing()),
        };
    }
    if let Some(q) = saved {
        // No fallback: quietly capturing from something else is exactly the
        // failure pinning exists to prevent.
        return match find(&of_kind, q) {
            Some(d) => Ok(Chosen { name: d.name.clone(), key: key_of(d), origin: Origin::Saved }),
            None => bail!(
                "the saved {what} [{q}] is not connected. Plug it back in, or pick another with \
                 {opt} <id|name>. Available:{}",
                listing()
            ),
        };
    }
    let pairs: Vec<(&str, Option<&str>)> = of_kind.iter().map(|d| (d.name.as_str(), d.id.as_deref())).collect();
    let idx = match kind {
        DeviceKind::Microphone => default_mic_index(&pairs, cfg!(target_os = "macos")),
        _ => first_real(&pairs),
    };
    match idx {
        Some(i) => {
            let d = of_kind[i];
            Ok(Chosen { name: d.name.clone(), key: key_of(d), origin: Origin::Auto })
        }
        None => bail!("no {what} found. Available:{}", listing()),
    }
}

/// Same order as `tp_media::find_device`: exact id, exact name, then name
/// substring, case-insensitive for names.
fn find<'a>(list: &[&'a DeviceInfo], q: &str) -> Option<&'a DeviceInfo> {
    let lq = q.to_lowercase();
    list.iter()
        .find(|d| d.id.as_deref() == Some(q))
        .or_else(|| list.iter().find(|d| d.name.to_lowercase() == lq))
        .or_else(|| list.iter().find(|d| d.name.to_lowercase().contains(&lq)))
        .copied()
}

/// Some providers list a "Default" pseudo-device that tracks the system
/// default; it is the thing being avoided, so it never counts.
fn is_pseudo_default(name: &str, id: Option<&str>) -> bool {
    name.trim().to_lowercase().starts_with("default") || id.is_some_and(|i| i.eq_ignore_ascii_case("default"))
}

fn first_real(list: &[(&str, Option<&str>)]) -> Option<usize> {
    list.iter().position(|(n, id)| !is_pseudo_default(n, *id))
}

/// On a Mac the built-in microphone, which stays put when headsets come and
/// go; elsewhere the first real device.
pub(crate) fn default_mic_index(list: &[(&str, Option<&str>)], mac: bool) -> Option<usize> {
    if mac {
        let builtin = list.iter().position(|(n, id)| {
            let n = n.to_lowercase();
            *id == Some("BuiltInMicrophoneDevice") || n.contains("macbook") || n.contains("built-in")
        });
        if builtin.is_some() {
            return builtin;
        }
    }
    first_real(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_prefers_builtin_over_airpods() {
        let list = [
            ("AirPods Pro", Some("AA-BB")),
            ("MacBook Air Microphone", Some("BuiltInMicrophoneDevice")),
        ];
        assert_eq!(default_mic_index(&list, true), Some(1));
        let by_name = [("AirPods Pro", Some("x")), ("Built-in Microphone", Some("y"))];
        assert_eq!(default_mic_index(&by_name, true), Some(1));
    }

    #[test]
    fn elsewhere_first_non_default() {
        let list = [("Default Input", None), ("Microphone (USB Audio)", Some("{1}")), ("CABLE Output", Some("{2}"))];
        assert_eq!(default_mic_index(&list, false), Some(1));
        assert_eq!(default_mic_index(&[("Default", Some("default"))], false), None);
    }

    #[test]
    fn mac_without_builtin_falls_back_to_first_real() {
        let list = [("Default", None), ("USB Mic", Some("u"))];
        assert_eq!(default_mic_index(&list, true), Some(1));
    }
}
