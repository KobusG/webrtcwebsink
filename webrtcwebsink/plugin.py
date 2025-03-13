import os
import json
import asyncio
import threading
import time
import logging
import aiohttp.web
# from http.server import HTTPServer
import socket
from termcolor import colored
from urllib.parse import quote

import gi
gi.require_version('Gst', '1.0')
gi.require_version('GstBase', '1.0')
gi.require_version('GstWebRTC', '1.0')
gi.require_version('GstSdp', '1.0')
from gi.repository import Gst, GObject, GstWebRTC, GstSdp

from .http_server import WebRTCHTTPHandler
from .signaling import SignalingServer

# Configure logging
logger = logging.getLogger('webrtcwebsink.plugin')
logging.basicConfig(
    format='%(asctime)s.%(msecs)03d %(levelname)-6s [ %(pathname)s:%(lineno)d ]    %(message)s',
    datefmt='%H:%M:%S',
    level=logging.INFO,
)

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
            'Video codec to use (default: h264)',
            'h264',
            GObject.ParamFlags.READWRITE
        ),
    }

    def __init__(self):
        Gst.Bin.__init__(self)

        # Initialize properties
        self.port = 8098
        self.ws_port = 8099
        self.bind_address = '0.0.0.0'
        self.stun_server = 'stun://stun.l.google.com:19302'
        self.video_codec = 'h264'

        # Initialize state
        self.http_server = None
        self.http_thread = None
        self.ssl_context = None
        self.signaling = None
        self.signaling_thread = None
        self.encoder = None
        self.payloader = None
        self.convert = None
        self.tee = None
        self.servers_started = False

        # Create internal elements
        self.setup_pipeline()

    def find_best_encoder(self, codec_name):
        """Find the highest-ranked encoder element that can produce the specified codec."""
        # Map codec names to their corresponding GStreamer caps
        logger.info(f"Finding best encoder for codec: {codec_name}")
        codec_caps = {
            'vp8': 'video/x-vp8',
            'h264': 'video/x-h264',
            'hevc': 'video/x-hevc',
            'av1': 'video/x-av1',
        }

        if codec_name not in codec_caps:
            logger.error(f"Unsupported codec: {codec_name}")
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
            return None, None

        # Find matching payloader based on encoder name and codec
        best_encoder = encoder_factories[0]
        encoder_name = best_encoder.get_name()
        logger.info(f"Selected encoder: {encoder_name} (rank: {best_encoder.get_rank()})")

        # Determine payloader based on codec
        payloader_map = {
            'vp8': 'rtpvp8pay',
            'h264': 'rtph264pay',
            'hevc': 'rtph265pay',
            'av1': 'rtpav1pay'
        }
        payloader = payloader_map.get(codec_name)

        return encoder_name, payloader

    def setup_pipeline(self):
        """Set up the internal GStreamer pipeline."""
        # Create elements
        self.convert = Gst.ElementFactory.make('videoconvert', 'convert')
        if not self.convert:
            raise Exception("Could not create videoconvert")

        # Create encoder and payloader from senders preference
        try:
            encoder_name, payloader_name = self.find_best_encoder(self.video_codec)
            # Create encoder and payloader elements for this client
            self.encoder = Gst.ElementFactory.make(encoder_name, 'encoder')
            self.payloader = Gst.ElementFactory.make(payloader_name, 'payloader')
            if not self.payloader or not self.encoder:
                logger.error(f"Could not create encoder/payloader {encoder_name}/{payloader_name}")
                return None
        except Exception as e:
            logger.error(f"Could not create encoder and payloader: {e}")
            return None

        # Configure codec-specific settings
        encoder_name = self.encoder.get_factory().get_name()
        if 'x264enc' in encoder_name:
            self.encoder.set_property('tune', 'zerolatency')
            self.encoder.set_property('speed-preset', 'ultrafast')
            self.encoder.set_property('key-int-max', 30)  # Keyframe every 1 second at 30fps
            self.encoder.set_property('bitrate', 2000)    # 2 Mbps
            logger.info(f"Configured {encoder_name} with low-latency settings")
        # Configure nvh264enc (NVIDIA)
        elif 'nvh264' in encoder_name:
            self.encoder.set_property('preset', 'low-latency')
            self.encoder.set_property('zerolatency', True)
            logger.info(f"Configured {encoder_name} with low-latency settings")
        # Configure vaapih264enc (Intel)
        elif 'vaapi' in encoder_name:
            self.encoder.set_property('rate-control', 'cbr')
            self.encoder.set_property('bitrate', 2000)  # 2 Mbps
            self.encoder.set_property('keyframe-period', 30)  # Keyframe every 1 second at 30fps
            logger.info(f"Configured {encoder_name} with low-latency settings")
        elif 'vpuenc_h264' in encoder_name:
            self.encoder.set_property('qp-max', 30)
            self.encoder.set_property('qp-min', 18)

        if self.video_codec == 'h264':
            self.payloader.set_property('config-interval', -1)
            self.payloader.set_property('aggregate-mode', 'zero-latency')
        elif self.video_codec == 'vp8':
            self.payloader.set_property('picture-id-mode', 2)
            self.payloader.set_property('config-interval', 1)
        elif self.video_codec == 'hevc':
            self.payloader.set_property('config-interval', -1)
            self.payloader.set_property('aggregate-mode', 'zero-latency')

        # Create tee to split the stream for multiple clients
        self.tee = Gst.ElementFactory.make('tee', 'tee')
        if not self.tee:
            raise Exception("Could not create tee")
        self.tee.set_property('allow-not-linked', True)  # Important for dynamic clients

        # Add elements to bin
        self.add(self.convert)
        self.add(self.encoder)
        self.add(self.payloader)
        self.add(self.tee)

        # Link elements
        self.convert.link(self.encoder)
        self.encoder.link(self.payloader)
        self.payloader.link(self.tee)

        # Create sink pad
        self.sink_pad = Gst.GhostPad.new('sink', self.convert.get_static_pad('sink'))
        self.add_pad(self.sink_pad)

    def create_webrtcbin(self):
        """Create a new WebRTCbin for a client connection."""
        logger.info("Creating new WebRTCbin")

        # Create a new webrtcbin
        webrtcbin = Gst.ElementFactory.make('webrtcbin', None)
        if not webrtcbin:
            logger.error("Failed to create WebRTCbin")
            return None

        # Configure the webrtcbin
        webrtcbin.set_property('stun-server', self.stun_server)

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

        # Get a source pad from the tee
        tee_src_pad = self.tee.get_request_pad("src_%u")
        if not tee_src_pad:
            logger.error("Failed to get source pad from tee")
            queue.set_state(Gst.State.NULL)
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(queue)
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
            self.remove(queue)
            self.remove(webrtcbin)
            return None

        # Link the queue to the webrtcbin
        ret = queue.link(webrtcbin)
        if not ret:
            logger.error("Failed to link queue to webrtcbin")
            tee_src_pad.unlink(queue_sink_pad)
            self.tee.release_request_pad(tee_src_pad)
            queue.set_state(Gst.State.NULL)
            webrtcbin.set_state(Gst.State.NULL)
            self.remove(queue)
            self.remove(webrtcbin)
            return None

        # Sync the element states with the parent
        queue.sync_state_with_parent()
        webrtcbin.sync_state_with_parent()

        logger.info("Successfully created and linked WebRTCbin")
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

    # def handle_message(self, message):
    #     """Handle GStreamer messages."""
    #     if message.type == Gst.MessageType.ERROR:
    #         error, debug = message.parse_error()
    #         logger.error(f"Error: {error.message}")
    #         logger.debug(f"Debug info: {debug}")
    #     return Gst.Bin.handle_message(self, message)

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

    def run_http_server(self, runner):
        """Run the HTTP server."""
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        loop.run_until_complete(runner.setup())
        site = aiohttp.web.TCPSite(runner, self.bind_address, self.port)
        loop.run_until_complete(site.start())
        loop.run_forever()

    def start_servers(self):
        """Start the HTTP and WebSocket servers."""
        # Create HTTP server socket with address reuse
        # sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        # sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        # sock.bind((self.bind_address, self.port))
        # sock.listen(1)

        # Create HTTP server with the bound socket
        # Set the WebSocket port in the handler class
        WebRTCHTTPHandler.ws_port = self.ws_port

        # self.http_server = HTTPServer(
        #     (self.bind_address, self.port),
        #     WebRTCHTTPHandler,
        #     bind_and_activate=False
        # )
        # self.http_server.socket = sock

        self.http_server = aiohttp.web.Application()
        self.http_server.router.add_get('/', lambda r: aiohttp.web.FileResponse(os.path.join(os.path.dirname(__file__), 'static', 'index.html')))
        self.http_server.router.add_get('/api/config', lambda r: aiohttp.web.json_response({'ws_port': self.ws_port}))
        self.http_server.router.add_static('/', os.path.join(os.path.dirname(__file__), 'static'))
        runner = aiohttp.web.AppRunner(self.http_server)
        self.http_thread = threading.Thread(target=self.run_http_server, args=(runner,))
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
        time.sleep(0.3)
        hostname = socket.gethostname()
        ip_address = socket.gethostbyname(hostname)
        print("video up at: " + colored(f"http://{hostname}.local:{self.port}", 'green', attrs=['underline']) + " or " + colored(f"http://{ip_address}:{self.port}", 'green', attrs=['underline']))

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
