use gst::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Camera,
    Microphone,
    AudioOutput,
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
    /// Backend that reported it, e.g. "wasapi2" or "mediafoundation".
    pub api: Option<String>,
    pub id: Option<String>,
    pub device: gst::Device,
}

impl DeviceInfo {
    /// The element is bound to this exact device, never to the system
    /// default, so a default-input change (AirPods connecting) cannot move it.
    pub fn create_element(&self) -> crate::Result<gst::Element> {
        let el = self.device.create_element(None)?;
        if let Some(id) = &self.id {
            // Providers already set these; reasserting is cheap insurance
            // against one that leaves the element on "default". Only an
            // empty value is filled in, since id formats differ per provider.
            for prop in ["unique-id", "device"] {
                // osxaudiosrc publishes unique-id read-only; writing it raises
                // a "not writable" critical, so only touch writable strings.
                if el.find_property(prop).is_some_and(|p| {
                    p.value_type() == String::static_type()
                        && p.flags().contains(gst::glib::ParamFlags::WRITABLE)
                        && !p.flags().contains(gst::glib::ParamFlags::CONSTRUCT_ONLY)
                }) {
                    let cur: Option<String> = el.property(prop);
                    if cur
                        .as_deref()
                        .is_none_or(|c| c.is_empty() || c == "default")
                    {
                        el.set_property(prop, id);
                    }
                }
            }
        }
        Ok(el)
    }
}

/// Windows exposes the same physical device through several providers. These
/// backends give the lowest latency, so they win when names collide.
fn api_rank(api: Option<&str>) -> u8 {
    match api {
        Some("wasapi2")
        | Some("mediafoundation")
        | Some("avfoundation")
        | Some("avf")
        | Some("osxaudio")
        | Some("pipewire") => 0,
        Some("wasapi") | Some("pulse") => 1,
        _ => 2,
    }
}

pub fn list_devices() -> crate::Result<Vec<DeviceInfo>> {
    crate::init()?;
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    monitor.add_filter(Some("Audio/Source"), None);
    monitor.add_filter(Some("Audio/Sink"), None);
    monitor.start()?;
    let raw = monitor.devices();
    monitor.stop();

    let mut out: Vec<DeviceInfo> = Vec::new();
    for device in raw {
        let class = device.device_class();
        // Providers disagree on order ("Source/Video" vs "Audio/Source").
        let parts: Vec<&str> = class.split('/').collect();
        let has = |p: &str| parts.contains(&p);
        let kind = if has("Video") && has("Source") {
            DeviceKind::Camera
        } else if has("Audio") && has("Source") {
            DeviceKind::Microphone
        } else if has("Audio") && has("Sink") {
            DeviceKind::AudioOutput
        } else {
            continue;
        };
        let props = device.properties();
        let get = |k: &str| props.as_ref().and_then(|p| p.get::<String>(k).ok());
        // wasapi2 lists every render endpoint again as a loopback capture
        // source; those are not microphones.
        if props
            .as_ref()
            .and_then(|p| p.get::<bool>("wasapi2.device.loopback").ok())
            == Some(true)
        {
            continue;
        }
        // osxaudio publishes no device.api, only its CoreAudio UID.
        let api = get("device.api").or_else(|| get("unique-id").map(|_| "osxaudio".to_string()));
        let info = DeviceInfo {
            name: device.display_name().to_string(),
            kind,
            api,
            id: get("device.id")
                .or_else(|| get("unique-id"))
                .or_else(|| get("avf.unique_id"))
                .or_else(|| get("device.strid"))
                .or_else(|| get("device.path")),
            device,
        };
        // Same name through a different backend is the same hardware twice;
        // same name through the same backend is two real devices.
        match out
            .iter_mut()
            .find(|d| d.kind == info.kind && d.name == info.name && d.api != info.api)
        {
            Some(existing) if api_rank(info.api.as_deref()) < api_rank(existing.api.as_deref()) => {
                *existing = info
            }
            Some(_) => {}
            None => out.push(info),
        }
    }
    Ok(out)
}

/// Picks a device by stable id first, then exact name, then name substring
/// (both case-insensitive). Never falls back to the default device: a miss
/// is an error naming what is available.
pub fn find_device(kind: DeviceKind, query: &str) -> crate::Result<DeviceInfo> {
    let all: Vec<DeviceInfo> = list_devices()?
        .into_iter()
        .filter(|d| d.kind == kind)
        .collect();
    pick_device(all, query)
}

fn pick_device(all: Vec<DeviceInfo>, query: &str) -> crate::Result<DeviceInfo> {
    let q = query.to_lowercase();
    let pos = all
        .iter()
        .position(|d| d.id.as_deref() == Some(query))
        .or_else(|| all.iter().position(|d| d.name.to_lowercase() == q))
        .or_else(|| all.iter().position(|d| d.name.to_lowercase().contains(&q)));
    match pos {
        Some(i) => Ok(all.into_iter().nth(i).expect("index from position")),
        None => {
            let avail: Vec<String> = all
                .iter()
                .map(|d| format!("{} [{}]", d.name, d.id.as_deref().unwrap_or("no id")))
                .collect();
            Err(crate::Error::DeviceNotFound(format!(
                "{query}; available: {}",
                if avail.is_empty() {
                    "none".to_string()
                } else {
                    avail.join(", ")
                }
            )))
        }
    }
}

/// Case-insensitive substring match on audio output names.
pub fn find_audio_output(name: &str) -> crate::Result<Option<DeviceInfo>> {
    let needle = name.to_lowercase();
    Ok(list_devices()?
        .into_iter()
        .find(|d| d.kind == DeviceKind::AudioOutput && d.name.to_lowercase().contains(&needle)))
}

/// VB-Cable's playback endpoint; whatever plays into it surfaces as the
/// "CABLE Output" microphone, which is the receiver's virtual mic for now.
pub fn find_vb_cable_input() -> crate::Result<Option<DeviceInfo>> {
    find_audio_output("CABLE Input")
}
