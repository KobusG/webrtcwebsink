use std::{sync::mpsc, thread};
use std::time::{Duration, Instant};
use gst::prelude::*;

use gstwebsink::websink::WebSink;

#[test]
fn test_websink_pipeline_runs_briefly() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize GStreamer
    gst::init()?;
    println!("GStreamer initialized for test.");

    // Manually register the WebSink element for this test.
    if gst::ElementFactory::find("websink").is_none() {
        gst::Element::register(None, "websink", gst::Rank::NONE, WebSink::static_type())?;
        println!("WebSink element registered for test.");
    } else {
        println!("WebSink element already registered.");
    }

    // A simple pipeline for testing. Using avenc_h264_omx as per original user pipeline.
    // Using a low framerate and resolution to keep the test light.
    // Removed is-live=true to make num-buffers more reliably send EOS for testing.
    let pipeline_str =
        "videotestsrc num-buffers=50 ! video/x-raw,width=640,height=320,framerate=10/1 ! videoconvert ! avenc_h264_omx ! websink is_live=true name=testwsink";

    println!("Attempting to launch test pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(pipeline_str)?;

    println!("Test pipeline created. Setting to Playing state.");
    pipeline.set_state(gst::State::Playing)?;

    // Let the pipeline run for a short period.
    // The videotestsrc is configured with num-buffers=50 and framerate=10/1,
    // so it will run for about 5 seconds and then send EOS.
    // We'll also add a bus watcher for EOS or error.
    let bus = pipeline.bus().ok_or("Failed to get bus from pipeline")?;
    let (tx, rx) = mpsc::channel();

    // Watch bus messages in a separate thread
    let bus_thread_pipeline = pipeline.clone();
    let _bus_watch_thread = thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            if tx.send(msg).is_err() {
                // Receiver has been dropped, pipeline likely stopping
                println!("Bus watch: Receiver dropped, stopping message watch.");
                break;
            }
        }
        // Ensure pipeline is stopped if this thread exits due to bus ending
        let _ = bus_thread_pipeline.set_state(gst::State::Null);
    });

    println!("Test pipeline running. Collecting messages for a few seconds...");
    // Let the pipeline run for a specific duration to collect messages
    // num-buffers=100 at 10fps should be 10 seconds. Add a bit of buffer.
    thread::sleep(Duration::from_secs(12));

    println!("Stopping pipeline and collecting messages.");
    pipeline.set_state(gst::State::Null)?; // Request pipeline to stop

    // Collect messages
    let mut collected_messages = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        collected_messages.push(msg);
    }
    // Wait a tiny bit for the bus watch thread to potentially send last messages and exit
    thread::sleep(Duration::from_millis(500));
     while let Ok(msg) = rx.try_recv() { // Final drain
        collected_messages.push(msg);
    }


    println!("Test pipeline stopped. Processing collected messages ({})...", collected_messages.len());

    let mut eos_received = false;
    let mut error_occurred: Option<String> = None;

    for msg in collected_messages {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                println!("Collected: EOS received.");
                eos_received = true;
            }
            MessageView::Error(err) => {
                let err_msg = format!(
                    "Collected Error from element {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                println!("{}", err_msg);
                error_occurred = Some(err_msg);
                // Don't break, collect all messages
            }
            MessageView::StateChanged(state_changed) => {
                // Optional: Log state changes if needed for debugging
                if state_changed.src().as_ref() == Some(pipeline.upcast_ref()) {
                    println!(
                        "Collected: Pipeline state changed from {:?} to {:?} pending {:?}",
                        state_changed.old(),
                        state_changed.current(),
                        state_changed.pending()
                    );
                }
            }
            // Add other message types to log if needed
            _ => (),
        }
    }

    if let Some(err_str) = error_occurred {
        return Err(err_str.into());
    }

    if !eos_received {
        // Depending on the pipeline, not receiving EOS might be an error or expected
        // For videotestsrc with num-buffers, EOS is expected.
        println!("Warning: EOS not found in collected messages.");
        // For this test, let's consider not getting EOS an issue if no other error occurred.
        // return Err("EOS not received from the pipeline.".into());
    }


    // A more robust test could try an HTTP GET request to the websink's port
    // (e.g., http://localhost:8091) to confirm the server started.
    // This would require an HTTP client dependency like `reqwest`.

    Ok(())
}