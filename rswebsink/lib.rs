// Basic GStreamer plugin for WebRTC streaming
use gst::glib;

mod websink;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Register our element with the plugin
    websink::register(plugin)
}

// Register the plugin using the proper GST plugin macro
gst::plugin_define!(
    websink,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);