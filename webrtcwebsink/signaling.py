import asyncio
import json
import websockets
import socket
import logging
import gi
gi.require_version('Gst', '1.0')
gi.require_version('GstWebRTC', '1.0')
gi.require_version('GstSdp', '1.0')
from gi.repository import Gst, GObject, GstWebRTC, GstSdp

# Configure logging
logger = logging.getLogger('webrtcwebsink.signaling')

# Enable more verbose logging for debugging
logger.setLevel(logging.DEBUG)

class SignalingServer:
    def __init__(self, webrtcbin_factory, host='0.0.0.0', port=8081):
        self.webrtcbin_factory = webrtcbin_factory
        self.host = host
        self.port = port
        self.server = None
        self.clients = {}  # Map of client_id -> {websocket, webrtcbin}
        self.running = False
        self.offer_in_progress = False
        self.loop = None

    async def run(self):
        """Start the signaling server."""
        logger.info(f"Starting server on {self.host}:{self.port}")
        if self.running:
            logger.warning("Server is already running!")
            return

        self.running = True
        try:
            # Create server
            logger.info("Starting WebSocket server...")
            server = await websockets.serve(
                self.handle_connection,
                self.host,
                self.port,
                reuse_address=True
            )
            logger.info("WebSocket server started successfully")

            # Keep the server running
            self.exit_future = asyncio.Future()
            await self.exit_future
            logger.info("Server stopped")
        except Exception as e:
            logger.error(f"Failed to start server: {e}")
        finally:
            self.running = False
            if hasattr(self, 'exit_future'):
                self.exit_future = None

    def start(self):
        """Start the server in a new event loop."""
        loop = asyncio.new_event_loop()
        self.loop = loop
        asyncio.set_event_loop(loop)
        try:
            loop.run_until_complete(self.run())
        finally:
            self.loop = None
            loop.close()

    def stop(self):
        """Stop the signaling server."""
        logger.info("Stopping server...")
        self.running = False

        if hasattr(self, 'exit_future') and not self.exit_future.done():
            logger.info("Triggering server shutdown...")
            asyncio.run_coroutine_threadsafe(
                self.close_all_connections(),
                self.exit_future.get_loop()
            )
            self.exit_future.set_result(None)

    async def close_all_connections(self):
        """Close all active WebSocket connections."""
        if self.clients:
            logger.info(f"Closing {len(self.clients)} active connections...")
            close_tasks = [client['websocket'].close() for client in self.clients.values()]
            await asyncio.gather(*close_tasks, return_exceptions=True)
            self.clients.clear()
            logger.info("All connections closed")

    async def handle_connection(self, websocket, path):
        """Handle a new WebSocket connection."""
        client_id = id(websocket)
        logger.info(f"New client connecting... (ID: {client_id})")
        try:
            # Wait for HELLO
            hello = await websocket.recv()
            logger.info(f"Received initial message from client {client_id}: {hello}")
            if not hello.startswith('HELLO'):
                logger.warning("Invalid hello message")
                await websocket.close(code=1002, reason='invalid protocol')
                return

            # Send back HELLO
            await websocket.send('HELLO')
            logger.info("Sent HELLO response to client {client_id}")

            # Wait for ROOM command
            room_cmd = await websocket.recv()
            logger.info(f"Received room command from client {client_id}: {room_cmd}")
            if not room_cmd.startswith('ROOM'):
                logger.warning("Invalid room command")
                await websocket.close(code=1002, reason='invalid protocol')
                return

            # Send ROOM_OK
            await websocket.send('ROOM_OK')
            logger.info(f"Sent ROOM_OK to client {client_id}")

            # Default codec preference
            codec_preference = None

            # Wait for optional codec preference
            try:
                codec_msg = await asyncio.wait_for(websocket.recv(), timeout=1.0)
                if codec_msg.startswith('CODEC'):
                    codec_preference = codec_msg.split(' ')[1].strip().lower()
                    logger.info(f"Client {client_id} requested codec: {codec_preference}")
                    # Force H.264 if the server is configured to use it
                    old_codec = codec_preference
                    logger.info(f"Forcing codec for client {client_id} from {old_codec} to h264")
                    if codec_preference != 'h264':
                        codec_preference = 'h264'
            except asyncio.TimeoutError:
                logger.debug(f"No codec preference received from client {client_id}")

            # Create a new WebRTCbin for this client
            webrtcbin = self.webrtcbin_factory(client_id, codec_preference)
            if not webrtcbin:
                logger.error(f"Failed to create WebRTCbin for client {client_id}")
                await websocket.close(code=1011, reason='internal server error')
                return

            # Connect to webrtcbin signals for this client
            webrtcbin.connect('on-negotiation-needed',
                             lambda element: self.on_negotiation_needed(element, client_id))
            webrtcbin.connect('on-ice-candidate',
                             lambda element, mlineindex, candidate:
                             self.on_ice_candidate(element, mlineindex, candidate, client_id))
            # Add client to our map
            self.clients[client_id] = {
                'websocket': websocket,
                'webrtcbin': webrtcbin
            }
            logger.info(f"Client {client_id} connected successfully")
            logger.info(f"Total active connections: {len(self.clients)}")

            # Trigger negotiation for this client
            logger.info(f"Triggering negotiation for client {client_id}")
            self.on_negotiation_needed(webrtcbin, client_id)

            async for message in websocket:
                if message.startswith('ROOM_PEER_MSG'):
                    jsonStart = message.find('{')
                    if jsonStart == -1:
                        continue
                    try:
                        data = json.loads(message[jsonStart:])
                        logger.debug(f"Received peer message from client {client_id}: {data}")

                        if 'answer' in data:
                            logger.info(f"Processing answer SDP from client {client_id}")
                            try:
                                _, sdpmsg = GstSdp.SDPMessage.new()
                                GstSdp.sdp_message_parse_buffer(bytes(data['answer']['sdp'].encode()), sdpmsg)
                                answer = GstWebRTC.WebRTCSessionDescription.new(GstWebRTC.WebRTCSDPType.ANSWER, sdpmsg)

                                # Use the client's WebRTCbin
                                promise = Gst.Promise.new()
                                webrtcbin.emit('set-remote-description', answer, promise)
                                promise.interrupt()
                                logger.info(f"Successfully set remote description for client {client_id}")
                            except Exception as e:
                                logger.error(f"Error setting remote description: {e}")

                        elif 'iceCandidate' in data:
                            logger.debug(f"Processing ICE candidate from client {client_id}")
                            try:
                                webrtcbin.emit('add-ice-candidate',
                                                data['iceCandidate']['sdpMLineIndex'],
                                                data['iceCandidate']['candidate'])
                                logger.debug(f"Successfully added ICE candidate from client {client_id}")
                            except Exception as e:
                                logger.error(f"Error adding ICE candidate: {e}")
                    except json.JSONDecodeError as e:
                        logger.error(f"Error parsing peer message: {e}")

        except Exception as e:
            logger.error(f"Error handling connection for client {client_id}: {e}")
        finally:
            # Clean up client resources
            if client_id in self.clients:
                del self.clients[client_id]
            logger.info(f"Client {client_id} disconnected")
            logger.info(f"Remaining active connections: {len(self.clients)}")

    def on_negotiation_needed(self, element, client_id=None):
        """Handle WebRTC negotiation-needed signal."""
        # Skip if client doesn't exist
        if client_id is None or client_id not in self.clients:
            logger.warning(f"Client {client_id} not found, skipping negotiation")
            return

        logger.info(f"Negotiation needed for client {client_id}, creating offer...")
        try:
            # Get the client's WebRTCbin
            webrtcbin = self.clients[client_id]['webrtcbin'] if client_id else element

            # Create offer
            promise = Gst.Promise.new_with_change_func(
                lambda promise, data: self.on_offer_created(promise, client_id),
                None
            )
            element.emit('create-offer', None, promise)
            logger.info(f"Offer creation initiated for client {client_id}")
        except Exception as e:
            logger.error(f"Error initiating offer creation for client {client_id}: {e}")

    def on_offer_created(self, promise, client_id):
        """Handle offer creation."""
        logger.info(f"Processing created offer for client {client_id}...")
        try:
            # Skip if client doesn't exist
            if client_id not in self.clients:
                logger.warning(f"Client {client_id} not found, skipping offer")
                return

            # Get the client's WebRTCbin and websocket
            client = self.clients[client_id]
            webrtcbin = client['webrtcbin']
            websocket = client['websocket']

            reply = promise.get_reply()
            offer = reply.get_value('offer')
            logger.debug(f"Got offer from promise for client {client_id}")

            promise = Gst.Promise.new()
            webrtcbin.emit('set-local-description', offer, promise)
            promise.interrupt()
            logger.debug(f"Local description set for client {client_id}")

            # Convert offer to string and store it
            offer_sdp = offer.sdp.as_text()
            logger.debug(f"Converted SDP for client {client_id}")

            # Send offer to this client
            message = json.dumps({
                'offer': {
                    'type': 'offer',
                    'sdp': offer_sdp
                }
            })
            message = f"ROOM_PEER_MSG server {message}"
            logger.info(f"Sending offer to client {client_id}")

            # Send the offer to the client
            asyncio.run_coroutine_threadsafe(
                self._send_message_to_client(websocket, message, f"Offer sent to client {client_id}"),
                self.loop
            )

        except Exception as e:
            logger.error(f"Error in offer creation for client {client_id}: {e}")

    def on_ice_candidate(self, element, mlineindex, candidate, client_id=None):
        """Handle new ICE candidate."""
        try:
            # Skip if client doesn't exist or loop is not available
            if client_id is None or client_id not in self.clients or not self.loop:
                logger.warning(f"Client {client_id} not found, skipping ICE candidate")
                return

            # Get the client's websocket
            websocket = self.clients[client_id]['websocket'] if client_id else None

            # Create ICE candidate message
            message = json.dumps({
                'iceCandidate': {
                    'candidate': candidate,
                    'sdpMLineIndex': mlineindex
                }
            })
            message = f"ROOM_PEER_MSG server {message}"
            logger.debug(f"Sending ICE candidate to client {client_id}")

            # Send the ICE candidate to the client
            asyncio.run_coroutine_threadsafe(
                self._send_message_to_client(websocket, message, f"ICE candidate sent to client {client_id}"),
                self.loop
            )

        except Exception as e:
            logger.error(f"Error sending ICE candidate to client {client_id}: {e}")

    async def _send_message_to_client(self, websocket, message, success_message):
        """Helper method to send a message to a client and handle errors."""
        try:
            if not websocket:
                logger.warning("No websocket provided, cannot send message")
                return
            await websocket.send(message)
            logger.debug(f"{success_message}")
        except Exception as e:
            logger.error(f"Error sending message: {e}")