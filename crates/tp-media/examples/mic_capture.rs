//! Lists microphones, opens the one matching the argument (id, exact name or
//! substring), shows which device the element is bound to, and counts the
//! audio it delivers for a few seconds.
//!
//! cargo run -p tp-media --example mic_capture -- "<id or name>"

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tp_media::gst::prelude::*;
use tp_media::*;

fn main() -> Result<()> {
    init()?;
    for d in list_devices()?
        .into_iter()
        .filter(|d| d.kind == DeviceKind::Microphone)
    {
        println!(
            "mic: {} [{}] id {}",
            d.name,
            d.api.as_deref().unwrap_or("?"),
            d.id.as_deref().unwrap_or("?")
        );
    }
    let Some(query) = std::env::args().nth(1) else {
        return Ok(());
    };
    let dev = find_device(DeviceKind::Microphone, &query)?;
    let src = dev.create_element()?;
    println!(
        "opened {} with {}",
        dev.name,
        src.factory()
            .map(|f| f.name().to_string())
            .unwrap_or_default()
    );
    for p in ["unique-id", "device"] {
        if let Some(spec) = src.find_property(p) {
            println!("  {p} = {:?}", src.property_value(spec.name()));
        }
    }

    let pipeline = gst::Pipeline::new();
    let sink = gst_app_sink()?;
    let bytes = Arc::new(AtomicU64::new(0));
    {
        let bytes = bytes.clone();
        sink.connect("new-sample", false, move |args| {
            let s = args[0].get::<gst::Element>().ok()?;
            let sample: Option<gst::Sample> = s.emit_by_name("pull-sample", &[]);
            if let Some(b) = sample.as_ref().and_then(|s| s.buffer()) {
                bytes.fetch_add(b.size() as u64, Ordering::Relaxed);
            }
            Some(gst::FlowReturn::Ok.to_value())
        });
    }
    pipeline.add_many([&src, &sink])?;
    src.link(&sink)?;
    pipeline.set_state(gst::State::Playing)?;
    std::thread::sleep(Duration::from_secs(3));
    if let Some(bus) = pipeline.bus()
        && let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error])
        && let gst::MessageView::Error(e) = msg.view()
    {
        println!("pipeline error: {} ({:?})", e.error(), e.debug());
    }
    for p in ["unique-id", "device"] {
        if let Some(spec) = src.find_property(p) {
            println!(
                "  after PLAYING {p} = {:?}",
                src.property_value(spec.name())
            );
        }
    }
    println!("captured {} bytes in 3 s", bytes.load(Ordering::Relaxed));
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

fn gst_app_sink() -> Result<gst::Element> {
    Ok(gst::ElementFactory::make("appsink")
        .property("emit-signals", true)
        .property("sync", false)
        .build()?)
}
