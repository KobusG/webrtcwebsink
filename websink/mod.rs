use gst::glib;
use gst::prelude::*;

// Module that contains the element implementation
mod imp;

// The WebSink element wrapped in a Rust safe interface
glib::wrapper! {
    pub struct WebSink(ObjectSubclass<imp::WebSink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

// Register the WebSink element with GStreamer
#[allow(dead_code)]
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "websink",
        gst::Rank::NONE,  // Make sure to import the correct prelude for this to work
        WebSink::static_type(),
    )
}