use gst::prelude::*;
use failure::Error;
use gst::glib;
mod websink;
// Import the plugin's registration function.
// Assuming the library target is named `gstwebsink` as per Cargo.toml
// and it has a `plugin_init` function or similar that registers elements.
// However, since `rswebsink` is a module within the `gstwebsink` library crate,
// and `gst::plugin_define!` handles registration, we just need to ensure
// GStreamer can find the plugin. For a local plugin within the same workspace,
// this is often handled by setting GST_PLUGIN_PATH or by GStreamer's discovery mechanisms
// if the plugin is installed.
// For this test application, we'll rely on GStreamer finding the plugin
// as if it were installed or `GST_PLUGIN_PATH` was set correctly.

fn main() {
    // Initialize GStreamer
    gst::init().expect("Failed to initialize gst_init");

    /* Enable stdout debug for websink category */
    gst::log::set_default_threshold(gst::DebugLevel::Warning);
    gst::log::set_threshold_for_name("websink", gst::DebugLevel::Debug);

    println!("🚀 Starting Rust WebSink test application");

    // Register the WebSink element with GStreamer
    gst::Element::register(None, "websink", gst::Rank::NONE, websink::WebSink::static_type()).unwrap();

    let main_loop = glib::MainLoop::new(None, false);

    start(&main_loop).expect("Failed to start");
}

fn start(main_loop: &glib::MainLoop) -> Result<(), Error> {

    let pls = "videotestsrc is-live=true ! video/x-raw,width=640,height=480,framerate=30/1 ! videoconvert ! x264enc tune=zerolatency ! websink name=wsink";
    let pipeline = gst::parse::launch(&pls).unwrap();
    let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .expect("Failed to set pipeline to `Playing`");

    let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();

    let main_loop_cloned = main_loop.clone();
    let bus = pipeline.bus().unwrap();
    let _bus_watch = bus
        .add_watch(move |_, msg| {
            use gst::MessageView;
            // println!("sender: {:?}", msg.view());
            match msg.view() {
                MessageView::Eos(..) => {
                    println!("Bus watch  Got eos");
                    main_loop_cloned.quit();
                }
                MessageView::Error(err) => {
                    println!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                _ => (),
            };
            glib::ControlFlow::Continue
        })
        .expect("failed to add bus watch");

    main_loop.run();
    pipeline
        .set_state(gst::State::Null)
        .expect("Failed to set pipeline to `Null`");
    println!("Done");
    Ok(())
}