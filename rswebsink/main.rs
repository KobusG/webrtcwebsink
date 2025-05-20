use gst::prelude::*;
use failure::Error;
use gst::glib;
use clap::Parser;
use gstwebsink::websink::WebSink;

#[derive(clap::Parser)]
#[command(version, about, long_about = None)]
struct Arguments {
    #[arg(short, long, default_value_t = false)]
    /// Produces verbose logs.
    verbose: bool,
}

fn main() {
    let args = Arguments::parse();
    register();
    let main_loop = glib::MainLoop::new(None, false);
    let pls = "videotestsrc ! video/x-raw,width=1280,height=640 ! videoconvert ! avenc_h264_omx ! websink is_live=true name=wsink";
    start(&main_loop, pls).expect("Failed to start");
}

fn register() {    // Initialize GStreamer
    gst::init().expect("Failed to initialize gst_init");

    // gst::log::remove_default_log_function();
    // /* Keep 1KB of logs per thread, removing old threads after 10 seconds */
    // gst::log::add_ring_buffer_logger(1024, 10);
    /* Enable all debug categories */
    gst::log::set_default_threshold(gst::DebugLevel::Warning);
    gst::log::set_threshold_for_name("websink", gst::DebugLevel::Debug);

    // Register the WebSink element with GStreamer
    gst::Element::register(None, "websink", gst::Rank::NONE, WebSink::static_type()).unwrap();
}

fn start(main_loop: &glib::MainLoop, pls: &str) -> Result<(), Error> {

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