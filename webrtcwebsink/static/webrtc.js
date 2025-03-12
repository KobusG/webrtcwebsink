// WebRTC and WebSocket connections
let pc = null;
let ws = null;

// DOM elements
const statusElement = document.getElementById('status');
const videoElement = document.getElementById('stream');
const errorElement = document.getElementById('error');

// Initialize video settings
function checkElements() {
    if (!statusElement || !videoElement) {
        console.error('Required DOM elements not found');
        return false;
    }
    return true;
}

function setStatus(text) {
    if (statusElement) {
        statusElement.textContent = text;
    }
    console.log('Status:', text);
}

function showError(text) {
    console.error('Error:', text);
    setStatus('Error: ' + text);
    if (errorElement) {
        errorElement.textContent = text;
        errorElement.style.display = 'block';
    }
}

function initVideo() {
    videoElement.playsInline = true;
    videoElement.autoplay = true;
    videoElement.muted = true;  // Mute to allow autoplay
}

// Detect supported codecs
async function detectSupportedCodecs() {
    try {
        const codecs = [
            { name: 'h264', mimeType: 'video/h264' },
            { name: 'vp8', mimeType: 'video/VP8' },
            { name: 'vp9', mimeType: 'video/VP9' },
            { name: 'av1', mimeType: 'video/AV1' }
        ];

        // Check which codecs are supported
        const supportedCodecs = [];
        for (const codec of codecs) {
            if (RTCRtpReceiver.getCapabilities) {
                const capabilities = RTCRtpReceiver.getCapabilities('video');
                const supported = capabilities.codecs.some(c =>
                    c.mimeType.toLowerCase() === codec.mimeType.toLowerCase());
                if (supported) {
                    supportedCodecs.push(codec.name);
                }
            } else {
                // Fallback for browsers without getCapabilities
                supportedCodecs.push(codec.name);
            }
        }

        console.log('Supported codecs:', supportedCodecs);

        // Prefer h264 if supported, otherwise use the first supported codec
        return supportedCodecs.includes('h264') ? 'h264' :
               (supportedCodecs.length > 0 ? supportedCodecs[0] : 'vp8');
    } catch (error) {
        console.warn('Error detecting codecs:', error);
        return 'vp8'; // Default to VP8 if detection fails
    }
}

function createPeerConnection() {
    if (pc) {
        console.log('Closing existing peer connection');
        pc.close();
    }

    const config = {
        iceServers: [{
            urls: 'stun:stun.l.google.com:19302'
        }]
    };

    pc = new RTCPeerConnection(config);
    console.log('Created new peer connection');

    // Set up video handling
    pc.ontrack = function(event) {
        console.log('Received track:', event.track.kind);
        if (event.track.kind === 'video') {
            console.log('Setting up video track');
            videoElement.srcObject = event.streams[0];
            console.log('Video stream set');

            event.track.onunmute = () => {
                console.log('Video track unmuted');
                videoElement.play()
                    .then(() => {
                        console.log('Video playing');
                        setStatus('Video streaming');
                    })
                    .catch(error => {
                        console.error('Error playing video:', error);
                        showError('Failed to play video: ' + error.message);
                    });
            };

            event.track.onended = () => {
                console.log('Video track ended');
                setStatus('Video stream ended');
                videoElement.srcObject = null;
            };
        }
    };

    // Connection monitoring
    pc.onconnectionstatechange = () => {
        console.log('Connection state changed:', pc.connectionState);
        setStatus('Connection: ' + pc.connectionState);
    };

    pc.oniceconnectionstatechange = () => {
        console.log('ICE connection state:', pc.iceConnectionState);
        setStatus('ICE connection: ' + pc.iceConnectionState);
    };

    pc.onicecandidate = (event) => {
        if (event.candidate) {
            console.log('Sending ICE candidate');
            ws.send(`ROOM_PEER_MSG server ${JSON.stringify({
                iceCandidate: event.candidate
            })}`);
        }
    };

    // Add transceiver in receive-only mode
    pc.addTransceiver('video', {direction: 'recvonly'});

    return pc;
}

async function start() {
    try {
        // Check elements and initialize video
        if (!checkElements()) {
            throw new Error('Required DOM elements not found');
        }
        initVideo();

        console.log('Starting WebRTC connection...');

        // Fetch configuration from server
        let wsPort;
        try {
            const response = await fetch('/api/config');
            if (!response.ok) {
                throw new Error(`HTTP error! status: ${response.status}`);
            }
            const config = await response.json();
            wsPort = config.ws_port;
            console.log('Fetched WebSocket port from server:', wsPort);
        } catch (error) {
            console.warn('Failed to fetch config, using default port:', error);
            wsPort = 8081; // Fallback to default port
        }

        // Connect to signaling server
        const wsUrl = `ws://${window.location.hostname}:${wsPort}`;
        console.log('Connecting to WebSocket server at:', wsUrl);
        ws = new WebSocket(wsUrl);

        ws.onopen = function() {
            console.log('WebSocket connection opened, sending HELLO');
            ws.send('HELLO');
            setStatus('Connecting to signaling server...');
        };

        ws.onclose = function(event) {
            console.log('WebSocket connection closed:', event.code, event.reason);
            setStatus('WebSocket disconnected');
            if (pc) {
                console.log('Closing peer connection');
                pc.close();
                pc = null;
            }
        };

        ws.onerror = function(error) {
            console.error('WebSocket error:', error);
            showError('WebSocket connection failed');
        };

        ws.onmessage = async function(event) {
            try {
                console.log('Received message:', event.data);
                const msg = event.data;

                if (msg === 'HELLO') {
                    console.log('Got HELLO from signaling server. Joining ROOM webrtc...');
                    ws.send('ROOM webrtc');
                    setStatus('Joining WebRTC room...');
                    return;
                }

                if (msg.startsWith('ROOM_OK')) {
                    console.log('Got ROOM OK from signaling server');
                    setStatus('Joined WebRTC room, waiting for stream...');

                    // Detect and send preferred codec
                    try {
                        const preferredCodec = await detectSupportedCodecs();
                        // Always send h264 to match server configuration
                        console.log('Detected codec:', preferredCodec, 'but forcing h264');
                        ws.send(`CODEC h264`);
                    } catch (error) {
                        console.warn('Error sending codec preference:', error);
                    }
                    return;
                }

                if (msg.startsWith('ROOM_PEER_MSG')) {
                    const jsonStart = msg.indexOf('{');
                    if (jsonStart === -1) {
                        console.warn('Invalid ROOM_PEER_MSG format:', msg);
                        return;
                    }

                    let data;
                    try {
                        data = JSON.parse(msg.slice(jsonStart));
                        console.log('Parsed peer message:', data);
                    } catch (error) {
                        console.error('Error parsing peer message:', error);
                        return;
                    }

                    if (data.offer) {
                        try {
                            console.log('Received offer:', data.offer);
                            setStatus('Received offer, creating answer...');

                            if (!pc) {
                                console.log('Creating new peer connection');
                                pc = createPeerConnection();
                            }

                            await pc.setRemoteDescription(new RTCSessionDescription(data.offer));
                            const answer = await pc.createAnswer({
                                offerToReceiveVideo: true
                            });
                            await pc.setLocalDescription(answer);

                            ws.send(`ROOM_PEER_MSG server ${JSON.stringify({
                                answer: answer
                            })}`);
                        } catch (error) {
                            console.error('Error during offer/answer:', error);
                            showError('Error during offer/answer: ' + error.message);
                        }
                    } else if (data.iceCandidate) {
                        try {
                            console.log('Received ICE candidate:', data.iceCandidate);
                            if (pc) {
                                await pc.addIceCandidate(data.iceCandidate);
                                console.log('Added ICE candidate successfully');
                            }
                        } catch (error) {
                            console.error('Error adding ICE candidate:', error);
                        }
                    }
                }
            } catch (error) {
                console.error('Error handling message:', error);
                showError(error.message);
            }
        };
    } catch (error) {
        console.error('Error:', error);
        showError(error.message);
    }
}

// Start when page loads
window.addEventListener('load', start);