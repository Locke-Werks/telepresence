use gst::prelude::*;

/// Sets a property from its string form, returning false instead of panicking
/// when the element lacks it or rejects the value. Encoder property names and
/// enum nicks drift between plugin versions, so every tuning knob is best effort.
pub fn try_set(el: &gst::Element, prop: &str, value: &str) -> bool {
    let Some(pspec) = el.find_property(prop) else {
        return false;
    };
    match gst::glib::Value::deserialize_with_pspec(value, &pspec) {
        Ok(v) => {
            el.set_property_from_value(prop, &v);
            true
        }
        Err(_) => false,
    }
}

pub fn make(factory: &str) -> crate::Result<gst::Element> {
    Ok(gst::ElementFactory::make(factory).build()?)
}

pub fn element_exists(factory: &str) -> bool {
    gst::ElementFactory::find(factory).is_some()
}

pub fn clock_time_to_us(t: Option<gst::ClockTime>) -> u64 {
    t.map(|t| t.useconds()).unwrap_or(0)
}

/// Non-blocking check for an error posted on a pipeline bus.
pub fn take_error(pipeline: &gst::Pipeline) -> crate::Result<()> {
    let Some(bus) = pipeline.bus() else {
        return Ok(());
    };
    if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error])
        && let gst::MessageView::Error(e) = msg.view()
    {
        return Err(crate::Error::Pipeline {
            source_name: e
                .src()
                .map(|s| s.path_string().to_string())
                .unwrap_or_default(),
            message: format!("{} ({:?})", e.error(), e.debug()),
        });
    }
    Ok(())
}
