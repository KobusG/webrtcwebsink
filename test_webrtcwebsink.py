#!/usr/bin/env python3
import sys
import gi
import signal
import threading
import os

gi.require_version('Gst', '1.0')
from gi.repository import Gst, GLib, GObject

# Enable more verbose logging
os.environ['GST_DEBUG'] = '4,webrtcwebsink:7'
Gst.init(None)

import webrtcwebsink

def plugin_init(plugin):
    return Gst.Element.register(plugin, "webrtcwebsink", Gst.Rank.NONE, webrtcwebsink.WebRTCWebSink)

def main():
    try:
        # Register the plugin
        if not Gst.Plugin.register_static(
            Gst.VERSION_MAJOR,
            Gst.VERSION_MINOR,
            "webrtcwebsink",
            "WebRTC Web Sink",
            plugin_init,
            "",
            "",
            "webrtcwebsink",
            "webrtcwebsink",
            ""
        ):
            raise RuntimeError("Failed to register webrtcwebsink plugin")
        print("Successfully registered webrtcwebsink plugin")

    except Exception as e:
        print(f"Error importing webrtcwebsink: {e}", file=sys.stderr)
        sys.exit(1)

    # List available elements to verify plugin registration
    registry = Gst.Registry.get()
    factory = registry.find_feature("webrtcwebsink", Gst.ElementFactory)
    if not factory:
        print("webrtcwebsink element not found in registry!", file=sys.stderr)
        sys.exit(1)
    print("Found webrtcwebsink element in registry")

    # Create the pipeline
    pipeline_str = '''
        videotestsrc is-live=true pattern=ball !
        videoconvert !
        video/x-raw,format=RGBA,width=640,height=480,framerate=30/1 !
        queue max-size-buffers=2 leaky=downstream !
        webrtcwebsink name=sink
    '''

    try:
        # Create and start the pipeline
        print("Creating pipeline...")
        pipeline = Gst.parse_launch(pipeline_str)
        if not pipeline:
            print("Failed to create pipeline", file=sys.stderr)
            sys.exit(1)

        # Set properties if needed
        print("Getting sink element...")
        sink = pipeline.get_by_name('sink')
        if not sink:
            print("Failed to find webrtcwebsink element", file=sys.stderr)
            sys.exit(1)

        # Set video codec property to test dynamic encoder selection
        print("Setting video codec to h264...")
        sink.set_property('video-codec', 'h264')
        print(f"Video codec set to: {sink.get_property('video-codec')}")

        print("Successfully configured sink element")

        # Create GLib main loop
        loop = GLib.MainLoop()

        # Handle pipeline messages
        def bus_call(bus, message, loop):
            t = message.type
            if t == Gst.MessageType.EOS:
                print("End-of-stream")
                loop.quit()
            elif t == Gst.MessageType.ERROR:
                err, debug = message.parse_error()
                print(f"Error: {err.message}", file=sys.stderr)
                if debug:
                    print(f"Debug info: {debug}", file=sys.stderr)
                loop.quit()
            elif t == Gst.MessageType.STATE_CHANGED:
                if message.src == pipeline:
                    old_state, new_state, pending_state = message.parse_state_changed()
                    print(f"Pipeline state changed from {old_state.value_nick} to {new_state.value_nick}")
            return True

        # Add bus watch
        bus = pipeline.get_bus()
        bus.add_signal_watch()
        bus.connect("message", bus_call, loop)

        # Handle Ctrl+C gracefully
        def signal_handler(sig, frame):
            print("\nStopping pipeline...")
            pipeline.set_state(Gst.State.NULL)
            loop.quit()

        signal.signal(signal.SIGINT, signal_handler)

        # Start playing
        print("Setting pipeline to PLAYING...")
        ret = pipeline.set_state(Gst.State.PLAYING)
        if ret == Gst.StateChangeReturn.FAILURE:
            print("Failed to start pipeline", file=sys.stderr)
            sys.exit(1)

        print("Pipeline is playing")
        print("Open your web browser to http://localhost:8080")
        print("Press Ctrl+C to stop")

        # Start the main loop
        try:
            loop.run()
        except Exception as e:
            print(f"Error in main loop: {e}", file=sys.stderr)
            pipeline.set_state(Gst.State.NULL)
            sys.exit(1)

    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == '__main__':
    main()