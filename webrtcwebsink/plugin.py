import os
import json
import asyncio
import threading
import time
import logging
from http.server import HTTPServer
import socket

import gi
gi.require_version('Gst', '1.0')
gi.require_version('GstBase', '1.0')
gi.require_version('GstWebRTC', '1.0')
gi.require_version('GstSdp', '1.0')
gi.require_version('GstPbutils', '1.0')  # Required for encoding profiles
from gi.repository import Gst, GObject, GstWebRTC, GstSdp, GstPbutils

from .http_server import WebRTCHTTPHandler
from .signaling import SignalingServer

# Configure logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger('webrtcwebsink.plugin')

# Enable more verbose logging for debugging
logging.getLogger('webrtcwebsink').setLevel(logging.DEBUG)

# Define the GObject type
class WebRTCWebSink(Gst.Bin, GObject.Object):
    """
    A GStreamer bin that acts as a WebRTC sink for streaming to web browsers.
    Includes an HTTP server for serving the client webpage and a WebSocket server
    for signaling.
    """

    # Register GObject type
    __gtype_name__ = 'WebRTCWebSink'

    # Register GStreamer plugin metadata
    __gstmetadata__ = (
        'WebRTC Web Sink',
        'Sink',
        'Stream video/audio to browsers using WebRTC',
        'Your Name'
    )

    # Register pad templates
    __gsttemplates__ = (
        Gst.PadTemplate.new(
            'sink',
            Gst.PadDirection.SINK,
            Gst.PadPresence.ALWAYS,
            Gst.Caps.from_string('video/x-raw,format={RGBA,RGB,I420,YV12,YUY2,UYVY,NV12,NV21}')
        ),
    )

    # Register properties
    __gproperties__ = {
        'port': (
            int,
            'HTTP Port',
            'Port for the HTTP server (default: 8080)',
            1,
            65535,
            8080,
            GObject.ParamFlags.READWRITE
        ),
        'ws-port': (
            int,
            'WebSocket Port',
            'Port for the WebSocket signaling server (default: 8081)',
            1,
            65535,
            8081,
            GObject.ParamFlags.READWRITE
        ),
        'bind-address': (
            str,
            'Bind Address',
            'Address to bind servers to (default: 0.0.0.0)',
            '0.0.0.0',
            GObject.ParamFlags.READWRITE
        ),
        'stun-server': (
            str,
            'STUN Server',
            'STUN server URI (default: stun://stun.l.google.com:19302)',
            'stun://stun.l.google.com:19302',
            GObject.ParamFlags.READWRITE
        ),
        'video-codec': (
            str,
            'Video Codec',
            'Video codec to use (default: vp8)',
            'vp8',
            GObject.ParamFlags.READWRITE
        ),
    }

    def __init__(self):
        Gst.Bin.__init__(self)

        # Initialize properties
        self.port = 8080
        self.ws_port = 8081
        self.bind_address = '0.0.0.0'
        self.stun_server = 'stun://stun.l.google.com:19302'
        self.video_codec = 'vp8'  # Match the default in __gproperties__

        # Initialize state
        self.http_server = None
        self.http_thread = None
        self.signaling = None
        self.signaling_thread = None
        self.encodebin = None
        self.convert = None
        self.tee = None
        self.servers_started = False
        self.payloader = None

        # Create internal elements
        self.setup_pipeline()

        # Dictionary to store client codec preferences
        self.client_codecs = {}

    def find_best_encoder(self, codec_name):
        """Find the highest-ranked encoder element that can produce the specified codec."""
        # Map codec names to their corresponding GStreamer caps
        logger.info(f"Finding best encoder for codec: {codec_name}")
        codec_caps = {
            'vp8': 'video/x-vp8',
            'h264': 'video/x-h264',
            'vp9': 'video/x-vp9',
            'av1': 'video/x-av1',
        }

        if codec_name not in codec_caps:
            logger.error(f"Unsupported codec: {codec_name}, falling back to vp8")
            return None, None

        target_caps = Gst.Caps.from_string(codec_caps[codec_name])

        # Find all encoder elements
        encoder_factories = []
        registry = Gst.Registry.get()
        factories = registry.get_feature_list(Gst.ElementFactory)
        for factory in factories:
            if ('encoder' in factory.get_name() or 'enc' in factory.get_name()) and 'encoder' in factory.get_metadata('klass').lower():
                # Check if this encoder can produce our target format
                for template in factory.get_static_pad_templates():
                    if template.direction == Gst.PadDirection.SRC:
                        template_caps = template.get_caps()
                        if template_caps.can_intersect(target_caps):
                            encoder_factories.append(factory)
                            break

        # Sort by rank
        encoder_factories.sort(key=lambda x: x.get_rank(), reverse=True)

        if not encoder_factories:
            logger.error(f"No encoder found for codec {codec_name}, falling back to vp8")
            if codec_name != 'vp8':
                return self.find_best_encoder('vp8')
            return None, None

        # Find matching payloader based on encoder name and codec
        best_encoder = encoder_factories[0]
        encoder_name = best_encoder.get_name()
        logger.info(f"Selected encoder: {encoder_name} (rank: {best_encoder.get_rank()})")

        # Determine payloader based on codec
        payloader_map = {
            'vp8': 'rtpvp8pay',
            'h264': 'rtph264pay',
            'vp9': 'rtpvp9pay',
            'av1': 'rtpav1pay'
        }
        payloader = payloader_map.get(codec_name)

        return encoder_name, payloader

    def setup_pipeline(self):
        """Set up the internal GStreamer pipeline."""
        # Create videoconvert element
        self.convert = Gst.ElementFactory.make('videoconvert', 'convert')
        if not self.convert:
            raise Exception("Could not create videoconvert")

        # Create tee element to split the stream for multiple clients
        self.tee = Gst.ElementFactory.make('tee', 'tee')
        if not self.tee:
            raise Exception("Could not create tee")
        self.tee.set_property('allow-not-linked', True)  # Important for dynamic clients

        # Add elements to bin and link them
        # Note: Each client will have its own encoder and payloader
        self.add(self.convert)
        self.add(self.tee)

        # Link convert directly to tee
        # Each client will get its own branch from the tee with encoder and payloader
        self.convert.link(self.tee)

        # Create sink pad
        self.sink_pad = Gst.GhostPad.new('sink', self.convert.get_static_pad('sink'))
        self.add_pad(self.sink_pad)

    def create_webrtcbin(self, client_id=None, codec_preference=None, **kwargs):
        """Create a new WebRTCbin for a client connection."""
        logger.info(f"Creating new WebRTCbin for client {client_id} with codec preference {codec_preference}")

        # Create a new webrtcbin
        webrtcbin = Gst.ElementFactory.make('webrtcbin', None)
        if not webrtcbin:
            logger.error("Failed to create WebRTCbin")
            return None

        # Configure the webrtcbin
        logger.debug(f"Setting STUN server to {self.stun_server}")
        webrtcbin.set_property('stun-server', self.stun_server)

        # Handle client codec preference
        codec_to_use = self.video_codec  # Default
        if client_id is not None and codec_preference is not None:
            logger.info(f"Client {client_id} prefers codec: {codec_preference}")
            self.client_codecs[client_id] = codec_preference
            codec_to_use = codec_preference
        else:
            logger.info(f"No codec preference for client {client_id}, using default: {self.video_codec}")

        # Create encoder and payloader for this client's preferred codec
        encoder_name, payloader_name = self.find_best_encoder(codec_to_use)
        logger.info(f"Using encoder {encoder_name} and payloader {payloader_name} for codec {codec_to_use} for client {client_id}")
        if not encoder_name or not payloader_name:
            logger.warning(f"Could not find encoder for {codec_to_use}, falling back to default")
            encoder_name, payloader_name = self.find_best_encoder(self.video_codec)
            if not encoder_name or not payloader_name:
                logger.error("Could not find any suitable encoder")
                webrtcbin.set_state(Gst.State.NULL)
                self.remove(webrtcbin)
                return None

        # Create encoder and payloader elements for this client
        encoder = Gst.ElementFactory.make(encoder_name, f'encoder-{client_id}')
        if not encoder:
            logger.error(f"Could not create encoder {encoder_name}")
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(webrtcbin)
            return None

        payloader = Gst.ElementFactory.make(payloader_name, f'payloader-{client_id}')
        if not payloader:
            logger.error(f"Could not create payloader {payloader_name}")
            encoder.set_state(Gst.State.NULL)
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(webrtcbin)
            return None

        # Add it to our bin
        self.add(webrtcbin)

        # Create a queue for this client
        queue = Gst.ElementFactory.make('queue', None)
        queue.set_property('leaky', 2)  # Leak downstream (old buffers)
        if not queue:
            logger.error("Failed to create queue")
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(webrtcbin)
            return None

        # Add the queue to our bin
        self.add(queue)

        # Add encoder and payloader to our bin
        self.add(encoder)
        self.add(payloader)

        # Get a source pad from the tee
        tee_src_pad = self.tee.get_request_pad("src_%u")
        if not tee_src_pad:
            logger.error("Failed to get source pad from tee")
            queue.set_state(Gst.State.NULL)
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(queue)
            self.remove(encoder)
            self.remove(payloader)
            self.remove(webrtcbin)
            return None

        # Get the sink pad from the queue
        queue_sink_pad = queue.get_static_pad("sink")

        # Link the tee to the queue
        ret = tee_src_pad.link(queue_sink_pad)
        if ret != Gst.PadLinkReturn.OK:
            logger.error(f"Failed to link tee to queue: {ret}")
            tee_src_pad.unlink(queue_sink_pad)
            self.tee.release_request_pad(tee_src_pad)
            queue.set_state(Gst.State.NULL)
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(encoder)
            self.remove(payloader)
            self.remove(queue)
            self.remove(webrtcbin)
            return None

        # Link the queue to the encoder to the payloader to the webrtcbin
        ret = queue.link(encoder)
        if not ret:
            logger.error("Failed to link queue to encoder")
            tee_src_pad.unlink(queue_sink_pad)
            self.tee.release_request_pad(tee_src_pad)
            queue.set_state(Gst.State.NULL)
            encoder.set_state(Gst.State.NULL)
            payloader.set_state(Gst.State.NULL)
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(queue)
            self.remove(encoder)
            self.remove(payloader)
            self.remove(webrtcbin)
            return None

        ret = encoder.link(payloader)
        if not ret:
            logger.error("Failed to link encoder to payloader")
            tee_src_pad.unlink(queue_sink_pad)
            self.tee.release_request_pad(tee_src_pad)
            queue.set_state(Gst.State.NULL)
            encoder.set_state(Gst.State.NULL)
            payloader.set_state(Gst.State.NULL)
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(queue)
            self.remove(encoder)
            self.remove(payloader)
            self.remove(webrtcbin)
            return None

        ret = payloader.link(webrtcbin)
        if not ret:
            logger.error("Failed to link payloader to webrtcbin")
            tee_src_pad.unlink(queue_sink_pad)
            self.tee.release_request_pad(tee_src_pad)
            queue.set_state(Gst.State.NULL)
            encoder.set_state(Gst.State.NULL)
            payloader.set_state(Gst.State.NULL)
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(queue)
            self.remove(encoder)
            self.remove(payloader)
            self.remove(webrtcbin)
            return None

        # Sync the element states with the parent
        queue.sync_state_with_parent()
        encoder.sync_state_with_parent()
        payloader.sync_state_with_parent()
        webrtcbin.sync_state_with_parent()

        # Configure codec-specific settings
        if codec_to_use == 'h264':
            # Configure H.264 encoder for low latency
            try:
                # Configure x264enc
                if 'x264' in encoder_name:
                    # encoder.set_property('tune', 'zerolatency')
                    # encoder.set_property('speed-preset', 'ultrafast')
                    encoder.set_property('key-int-max', 30)  # Keyframe every 1 second at 30fps
                    encoder.set_property('bitrate', 2000)    # 2 Mbps
                    logger.info(f"Configured {encoder_name} with low-latency settings")
                # Configure nvh264enc (NVIDIA)
                elif 'nvh264' in encoder_name:
                    try:
                        encoder.set_property('preset', 'low-latency')
                    except Exception as e:
                        logger.warning(f"Could not set preset property on {encoder_name}: {e}")
                        # Try alternative properties for older NVIDIA encoder versions
                        encoder.set_property('rc-mode', 'cbr')  # Constant bitrate for low latency
                    encoder.set_property('zerolatency', True)
                    logger.info(f"Configured {encoder_name} with low-latency settings")
                # Configure vaapih264enc (Intel)
                elif 'vaapi' in encoder_name:
                    try:
                        encoder.set_property('rate-control', 'cbr')
                        encoder.set_property('bitrate', 2000)  # 2 Mbps
                    except Exception as e:
                        logger.warning(f"Could not set all properties on {encoder_name}: {e}")
                    # Set low latency properties
                    encoder.set_property('keyframe-period', 30)  # Keyframe every 1 second at 30fps
                    logger.info(f"Configured {encoder_name} with low-latency settings")
            except Exception as e:
                logger.warning(f"Could not set all properties on H.264 encoder: {e}")
            payloader.set_property('config-interval', -1)
            payloader.set_property('aggregate-mode', 'zero-latency')

        logger.info(f"Successfully created and linked WebRTCbin with codec {codec_to_use} for client {client_id}")

        # Add a probe to the payloader source pad to monitor the data flow
        payloader_src_pad = payloader.get_static_pad('src')
        if payloader_src_pad:
            payloader_src_pad.add_probe(Gst.PadProbeType.BUFFER,
                                        lambda pad, info: logger.debug(f"Data flowing through payloader for codec {codec_to_use}") or Gst.PadProbeReturn.OK,
                                        None)

        return webrtcbin

    def do_get_property(self, prop):
        """Handle property reads."""
        if prop.name == 'port':
            return self.port
        elif prop.name == 'ws-port':
            return self.ws_port
        elif prop.name == 'bind-address':
            return self.bind_address
        elif prop.name == 'stun-server':
            return self.stun_server
        elif prop.name == 'video-codec':
            return self.video_codec
        else:
            raise AttributeError(f'Unknown property {prop.name}')

    def do_set_property(self, prop, value):
        """Handle property writes."""
        if prop.name == 'port':
            self.port = value
        elif prop.name == 'ws-port':
            self.ws_port = value
        elif prop.name == 'bind-address':
            self.bind_address = value
        elif prop.name == 'stun-server':
            self.stun_server = value
            # This will apply to new WebRTCbins created
        elif prop.name == 'video-codec':
            self.video_codec = value
            logger.info(f"Default video codec set to {self.video_codec}, will be used for new connections without specific preferences")
        else:
            raise AttributeError(f'Unknown property {prop.name}')

    def handle_message(self, message):
        """Handle GStreamer messages."""
        if message.type == Gst.MessageType.ERROR:
            error, debug = message.parse_error()
            logger.error(f"Error: {error.message}")
            logger.debug(f"Debug info: {debug}")
        return Gst.Bin.handle_message(self, message)

    def do_change_state(self, transition):
        """Handle state changes."""
        if transition == Gst.StateChange.NULL_TO_READY:
            # Start servers only if they haven't been started yet
            if not self.servers_started:
                try:
                    self.start_servers()
                    self.servers_started = True
                except Exception as e:
                    logger.error(f"Failed to start servers: {e}")
                    return Gst.StateChangeReturn.FAILURE
        elif transition == Gst.StateChange.READY_TO_NULL:
            # Stop servers only if they are running
            if self.servers_started:
                self.stop_servers()
                self.servers_started = False

        return Gst.Bin.do_change_state(self, transition)

    def start_servers(self):
        """Start the HTTP and WebSocket servers."""
        # Create HTTP server socket with address reuse
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind((self.bind_address, self.port))
        sock.listen(1)

        # Create HTTP server with the bound socket
        # Set the WebSocket port in the handler class
        WebRTCHTTPHandler.ws_port = self.ws_port

        self.http_server = HTTPServer(
            (self.bind_address, self.port),
            WebRTCHTTPHandler,
            bind_and_activate=False
        )
        self.http_server.socket = sock

        self.http_thread = threading.Thread(target=self.http_server.serve_forever)
        self.http_thread.daemon = True
        self.http_thread.start()

        # Start WebSocket signaling server
        self.signaling = SignalingServer(
            self.create_webrtcbin,
            host=self.bind_address,
            port=self.ws_port
        )
        self.signaling_thread = threading.Thread(target=self.signaling.start)
        self.signaling_thread.daemon = True
        self.signaling_thread.start()

        # Wait a bit for the server to start
        time.sleep(0.5)

    def stop_servers(self):
        """Stop the HTTP and WebSocket servers."""
        if self.http_server:
            self.http_server.shutdown()
            self.http_server = None
            self.http_thread = None

        if self.signaling:
            self.signaling.stop()
            self.signaling = None
            self.signaling_thread = None

# Register the GObject type
GObject.type_register(WebRTCWebSink)
__gstelementfactory__ = ("webrtcwebsink", Gst.Rank.NONE, WebRTCWebSink)