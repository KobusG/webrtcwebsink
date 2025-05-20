use gst::glib;
use gst::prelude::*;

// Module that contains the element implementation
pub mod imp; // Make the imp module public

// The WebSink element wrapped in a Rust safe interface
glib::wrapper! {
    pub struct WebSink(ObjectSubclass<imp::WebSink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

// Register the WebSink element with GStreamer
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "websink",
        gst::Rank::NONE,  // Make sure to import the correct prelude for this to work
        WebSink::static_type(),
    )
}