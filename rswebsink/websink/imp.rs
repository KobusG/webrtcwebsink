use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use std::sync::{Arc, Mutex};
use std::sync::LazyLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use uuid::Uuid;

use warp::Filter;
use rust_embed::RustEmbed;
use std::borrow::Cow;

// WebRTC imports
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::media::Sample;

// Color codes for terminal output
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

// Debug category for the WebSink element
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "websink",
        gst::DebugColorFlags::empty(),

        Some("webrtc streaming sink element"),
    )
});

// Default values for properties
const DEFAULT_PORT: u16 = 8091;
const DEFAULT_STUN_SERVER: &str = "stun:stun.l.google.com:19302";

// Types for WebRTC signaling
#[derive(Serialize, Deserialize, Debug)]
pub struct SessionRequest {
    pub offer: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SessionResponse {
    pub answer: serde_json::Value,
    pub session_id: String,
}

// Property value storage
#[derive(Debug, Clone)]
struct Settings {
    port: u16,
    stun_server: String,
    is_live: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            stun_server: String::from(DEFAULT_STUN_SERVER),
            is_live: false,
        }
    }
}

#[derive(RustEmbed)]
#[folder = "rswebsink/static/"] // Path relative to the Cargo.toml of the rswebsink crate
struct Asset;

// Custom error for session handling
#[derive(Debug)]
struct SessionError(String);
impl warp::reject::Reject for SessionError {}

// Handle WebRTC session request (create peer connection and answer)
async fn handle_session_request(req: SessionRequest) -> Result<SessionResponse, Box<dyn std::error::Error + Send + Sync>> {
    gst::info!(CAT, "🎯 Processing WebRTC session request");

    // Create a MediaEngine and API
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Configure WebRTC with STUN server
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new peer connection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);
    gst::info!(CAT, "📞 Created new peer connection");

    // Create and add video track
    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "websink".to_owned(),
    ));

    // Add the track to the peer connection
    let _rtp_sender = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;
    gst::info!(CAT, "🎥 Added video track to peer connection");

    // Parse the offer from the request
    let offer: RTCSessionDescription = serde_json::from_value(req.offer)?;
    gst::info!(CAT, "📨 Parsed offer from client");

    // Set remote description
    peer_connection.set_remote_description(offer).await?;
    gst::info!(CAT, "🔗 Set remote description");

    // Create answer
    let answer = peer_connection.create_answer(None).await?;
    gst::info!(CAT, "📤 Created answer");

    // Set local description
    peer_connection.set_local_description(answer).await?;
    gst::info!(CAT, "🏠 Set local description");

    // Wait for ICE gathering to complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;
    let _ = gather_complete.recv().await;
    gst::info!(CAT, "🧊 ICE gathering completed");

    // Get the final answer with ICE candidates
    let final_answer = peer_connection.local_description().await
        .ok_or("Failed to get local description")?;

    // Generate session ID
    let session_id = Uuid::new_v4().to_string();

    // Serialize answer to JSON
    let answer_json = serde_json::to_value(&final_answer)?;

    let response = SessionResponse {
        answer: answer_json,
        session_id: session_id.clone(),
    };

    gst::info!(CAT, "✅ WebRTC session established with ID: {}", session_id);
    Ok(response)
}

// Element state containing HTTP server and WebRTC components
struct State {
    runtime: Option<Runtime>,
    server_handle: Option<tokio::task::JoinHandle<()>>,
    peer_connections: HashMap<String, Arc<webrtc::peer_connection::RTCPeerConnection>>,
    unblock_tx: Option<mpsc::Sender<i32>>,
    unblock_rx: Option<mpsc::Receiver<i32>>,
    // WebRTC components
    webrtc_api: Option<webrtc::api::API>,
    video_track: Option<Arc<TrackLocalStaticSample>>,
    webrtc_config: Option<RTCConfiguration>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            runtime: None,
            server_handle: None,
            peer_connections: HashMap::new(),
            unblock_tx: None,
            unblock_rx: None,
            webrtc_api: None,
            video_track: None,
            webrtc_config: None,
        }
    }
}

// Element that keeps track of everything
pub struct WebSink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    num_peers: AtomicI32,
}

// Default implementation for our element
impl Default for WebSink {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            num_peers: AtomicI32::new(0),
        }
    }
}

// Implementation of GObject virtual methods for our element
#[glib::object_subclass]
impl ObjectSubclass for WebSink {
    const NAME: &'static str = "WebSink";
    type Type = super::WebSink;
    type ParentType = gst_base::BaseSink;
}

// Implementation of GObject methods
impl ObjectImpl for WebSink {
    fn properties() -> &'static [glib::ParamSpec] {
        use once_cell::sync::Lazy;
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::builder("port")
                    .nick("HTTP Port")
                    .blurb("Port to use for the HTTP server (0 for auto)")
                    .minimum(0)
                    .maximum(65535)
                    .default_value(DEFAULT_PORT as u32)
                    .build(),
                glib::ParamSpecString::builder("stun-server")
                    .nick("STUN Server")
                    .blurb("STUN server to use for WebRTC (empty for none)")
                    .default_value(DEFAULT_STUN_SERVER)
                    .build(),
                glib::ParamSpecBoolean::builder("is-live")
                    .nick("Live Mode")
                    .blurb("Whether to block Render without peers (default: false)")
                    .default_value(false)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "port" => {
                let mut settings = self.settings.lock().unwrap();
                let port = value.get::<u32>().expect("type checked upstream") as u16;
                gst::info!(CAT, "Changing port from {} to {}", settings.port, port);
                settings.port = port;
            }
            "stun-server" => {
                let mut settings = self.settings.lock().unwrap();
                let stun_server = value.get::<Option<String>>().expect("type checked upstream").unwrap_or_else(|| DEFAULT_STUN_SERVER.to_string());
                gst::info!(CAT, "Changing stun-server from {} to {}", settings.stun_server, stun_server);
                settings.stun_server = stun_server;
            }
            "is-live" => {
                let mut settings = self.settings.lock().unwrap();
                let is_live = value.get::<bool>().expect("type checked upstream");
                gst::info!( CAT, "Changing is-live from {} to {}", settings.is_live, is_live);
                settings.is_live = is_live;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "port" => {
                let settings = self.settings.lock().unwrap();
                glib::Value::from(&(settings.port as u32))
            }
            "stun-server" => {
                let settings = self.settings.lock().unwrap();
                settings.stun_server.to_value()
            }
            "is-live" => {
                let settings = self.settings.lock().unwrap();
                settings.is_live.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

// Implementation of GstObject methods
impl GstObjectImpl for WebSink {}

// Implementation of Element methods
impl ElementImpl for WebSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        use once_cell::sync::Lazy;
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WebRTC Sink",
                "Sink/Network",
                "Stream H264 video to web browsers using WebRTC",
                "Videology Inc <info@videology.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        use once_cell::sync::Lazy;
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

// Implementation of BaseSink methods
impl BaseSinkImpl for WebSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::info!(CAT, "🚀 Starting WebSink");

        // Initialize Tokio runtime
        gst::debug!(CAT, "⚙️ Initializing Tokio runtime");
        let runtime = match Runtime::new() {
            Ok(rt) => {
                gst::info!(CAT, "✅ Tokio runtime created successfully");
                rt
            },
            Err(err) => {
                gst::error!(CAT, "❌ Failed to create Tokio runtime: {}", err);
                return Err(gst::error_msg!(gst::ResourceError::Failed, ["Failed to create Tokio runtime: {}", err]));
            }
        };

        // Setup an unblock channel for live mode
        let (tx, rx) = mpsc::channel(1);
        gst::info!(CAT, "📺 Created mpsc channel for live mode signaling");

        // Initialize WebRTC API
        gst::debug!(CAT, "🌐 Initializing WebRTC API");
        let webrtc_api = runtime.block_on(async {
            let mut m = MediaEngine::default();
            m.register_default_codecs()?;

            let mut registry = Registry::new();
            registry = register_default_interceptors(registry, &mut m)?;

            let api = APIBuilder::new()
                .with_media_engine(m)
                .with_interceptor_registry(registry)
                .build();

            Ok::<webrtc::api::API, webrtc::Error>(api)
        }).map_err(|err| {
            gst::error!(CAT, "❌ Failed to create WebRTC API: {}", err);
            gst::error_msg!(gst::ResourceError::Failed, ["Failed to create WebRTC API: {}", err])
        })?;
        gst::info!(CAT, "✅ WebRTC API initialized successfully");

        // Create video track
        gst::debug!(CAT, "🎥 Creating video track for H.264");
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "websink".to_owned(),
        ));
        gst::info!(CAT, "✅ Video track created successfully");

        // Configure WebRTC
        let settings = self.settings.lock().unwrap();
        let mut webrtc_config = RTCConfiguration::default();
        if !settings.stun_server.is_empty() {
            webrtc_config.ice_servers = vec![RTCIceServer {
                urls: vec![settings.stun_server.clone()],
                ..Default::default()
            }];
            gst::info!(CAT, "🌐 STUN server configured: {}", settings.stun_server);
        } else {
            gst::info!(CAT, "⚠️ No STUN server configured");
        }
        let port = settings.port;
        drop(settings);

        let mut state = self.state.lock().unwrap();
        state.runtime = Some(runtime);
        state.unblock_tx = Some(tx);
        state.unblock_rx = Some(rx);
        state.webrtc_api = Some(webrtc_api);
        state.video_track = Some(video_track);
        state.webrtc_config = Some(webrtc_config);

        // Start HTTP server
        gst::info!(CAT, "🌐 Starting HTTP server on port {}", port);
        let rt = state.runtime.as_ref().expect("Runtime should be initialized");
        let server_handle = self.start_http_server(port, rt);

        state.server_handle = Some(server_handle);
        gst::info!(CAT, "✅ WebSink started successfully");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::info!(CAT, "🛑 Stopping WebSink");

        // Clean up resources
        let mut state = self.state.lock().unwrap();

        // Stop the HTTP server
        if let Some(handle) = state.server_handle.take() {
            gst::info!(CAT, "🌐 Aborting HTTP server task...");
            handle.abort();
            // Optionally, could await the handle here if running in a context that allows it,
            // but abort is generally sufficient for cleanup.
            gst::info!(CAT, "✅ HTTP server task aborted.");
        } else {
            gst::debug!(CAT, "🌐 No HTTP server handle to abort");
        }

        // Clear peer connections
        let peer_count = state.peer_connections.len();
        state.peer_connections.clear();
        gst::info!(CAT, "👥 Cleared {} peer connections", peer_count);

        // Reset state
        state.unblock_tx = None;
        state.unblock_rx = None;
        state.runtime = None;
        state.webrtc_api = None;
        state.video_track = None;
        state.webrtc_config = None;
        gst::debug!(CAT, "🧹 Reset all state components");

        // Reset peer count
        self.num_peers.store(0, Ordering::SeqCst);
        gst::debug!(CAT, "🔢 Reset peer count to 0");

        gst::info!(CAT, "✅ WebSink stopped successfully");
        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Get the number of connected peers
        let num_peers = self.num_peers.load(Ordering::SeqCst);
        let settings_guard = self.settings.lock().unwrap();

        gst::trace!(CAT, "🎬 Render called - buffer size: {} bytes, peers: {}",
                   buffer.size(), num_peers);

        // In live mode, we skip rendering if no peers are connected
        if settings_guard.is_live && num_peers == 0 {
            gst::trace!(CAT, "⏭️ No peers connected in live mode, skipping buffer");
            return Ok(gst::FlowSuccess::Ok);
        }
        drop(settings_guard);

        // Map the buffer to get the data
        let map = buffer.map_readable().map_err(|_| {
            gst::error!(CAT, "❌ Failed to map buffer");
            gst::FlowError::Error
        })?;

        let data = map.as_slice();

        // Send to video track if we have peers
        if num_peers > 0 {
            gst::debug!(CAT, "📹 Sending {} bytes to {} peers", data.len(), num_peers);

            let state = self.state.lock().unwrap();
            if let Some(video_track) = &state.video_track {
                let track_clone = Arc::clone(video_track);
                let data_copy = bytes::Bytes::copy_from_slice(data);
                let duration = buffer.duration().unwrap_or_else(|| gst::ClockTime::from_nseconds(33_333_333)); // Default 30fps

                gst::trace!(CAT, "⏱️ Buffer duration: {} ns", duration.nseconds());

                // Use the runtime to send the sample
                if let Some(runtime) = &state.runtime {
                    runtime.spawn(async move {
                        let sample = Sample {
                            data: data_copy,
                            duration: Duration::from_nanos(duration.nseconds()),
                            ..Default::default()
                        };

                        gst::trace!(CAT, "🚀 Spawned async task to write sample to WebRTC track");

                        if let Err(e) = track_clone.write_sample(&sample).await {
                            gst::error!(CAT, "❌ Failed to write sample to WebRTC track: {}", e);
                        } else {
                            gst::trace!(CAT, "✅ Successfully wrote sample to WebRTC track");
                        }
                    });
                } else {
                    gst::error!(CAT, "❌ No Tokio runtime available for async sample writing");
                }
            } else {
                gst::warning!(CAT, "⚠️ No video track available for sample writing");
            }
        } else {
            gst::trace!(CAT, "👥 No peers connected, not sending video data");
        }

        gst::trace!(CAT, "✅ Rendered buffer with {} bytes to {} peers", data.len(), num_peers);
        Ok(gst::FlowSuccess::Ok)
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, "🔓 Unlocking");

        // Signal to unblock the render method
        let state = self.state.lock().unwrap();
        if let Some(tx) = &state.unblock_tx {
            match tx.try_send(-1) {
                Ok(_) => gst::debug!(CAT, "📤 Sent unlock signal via mpsc channel"),
                Err(e) => gst::warning!(CAT, "⚠️ Failed to send unlock signal: {:?}", e),
            }
        } else {
            gst::warning!(CAT, "⚠️ No mpsc sender available for unlock signal");
        }

        Ok(())
    }
}

impl WebSink {
    fn start_http_server(&self, port: u16, rt: &Runtime) -> tokio::task::JoinHandle<()> {
        gst::info!(CAT, "Starting HTTP server on port {}", port);

        rt.spawn(async move {
            // API session handler - now with actual WebRTC signaling
            let api_session = warp::path!("api" / "session")
                .and(warp::post())
                .and(warp::body::json())
                .and_then(|body: SessionRequest| async move {
                    gst::info!(CAT, "🔗 Received WebRTC session request");
                    gst::debug!(CAT, "📨 Session request body: {:?}", body);

                    match handle_session_request(body).await {
                        Ok(response) => {
                            gst::info!(CAT, "✅ Successfully handled WebRTC session request");
                            Ok(warp::reply::json(&response))
                        },
                        Err(e) => {
                            gst::error!(CAT, "❌ Failed to handle WebRTC session request: {}", e);
                            Err(warp::reject::custom(SessionError(e.to_string())))
                        }
                    }
                });

            let static_assets = warp::path::tail().and_then(|tail: warp::path::Tail| async move {
                let path = tail.as_str();
                let path_to_serve = if path.is_empty() || path == "/" {
                    "index.html"
                } else {
                    path
                };

                gst::debug!(CAT, "🌐 Static asset request for: {}", path_to_serve);

                match Asset::get(path_to_serve) {
                    Some(content) => {
                        let mime = mime_guess::from_path(path_to_serve).first_or_octet_stream();
                        let body: Cow<'static, [u8]> = content.data;
                        gst::debug!(CAT, "✅ Serving static asset: {} ({} bytes, mime: {})",
                                   path_to_serve, body.len(), mime.as_ref());
                        let response = warp::http::Response::builder()
                            .header("Content-Type", mime.as_ref())
                            .body(body)
                            .map_err(|_| warp::reject::custom(ServeError))?;
                        Ok(response)
                    }
                    None => {
                        gst::warning!(CAT, "❌ Static asset not found: {}", path_to_serve);
                        Err(warp::reject::not_found())
                    }
                }
            });

            let routes = api_session.or(static_assets);

            gst::info!(CAT, "HTTP server starting on http://0.0.0.0:{}", port);
            println!("{}HTTP server starting on http://localhost:{}{}", GREEN, port, RESET);

            warp::serve(routes).run(([0, 0, 0, 0], port)).await;
            gst::info!(CAT, "HTTP server on port {} stopped.", port);
        })
    }

    fn update_peer_connections(&self, peer_id: String, pc: Option<Arc<webrtc::peer_connection::RTCPeerConnection>>, add: bool) {
        gst::debug!(CAT, "🔄 Updating peer connections - peer: {}, add: {}", peer_id, add);

        let mut state = self.state.lock().unwrap();

        if add {
            if let Some(connection) = pc {
                state.peer_connections.insert(peer_id.clone(), connection);
                gst::info!(CAT, "➕ Added peer connection: {}", peer_id);
            }
        } else {
            if let Some(_) = state.peer_connections.remove(&peer_id) {
                gst::info!(CAT, "➖ Removed peer connection: {}", peer_id);
            } else {
                gst::warning!(CAT, "⚠️ Tried to remove non-existent peer: {}", peer_id);
            }
        }

        let count = state.peer_connections.len() as i32;
        self.num_peers.store(count, Ordering::SeqCst);
        gst::info!(CAT, "👥 Client count changed: {} connected clients", count);

        // Send notification on peer change channel (non-blocking)
        if let Some(tx) = &state.unblock_tx {
            match tx.try_send(count) {
                Ok(_) => gst::debug!(CAT, "📤 Sent peer count update ({}) via mpsc channel", count),
                Err(e) => gst::warning!(CAT, "⚠️ Failed to send peer count update: {:?}", e),
            }
        } else {
            gst::warning!(CAT, "⚠️ No mpsc sender available for peer count update");
        }
    }
}
#[derive(Debug)]
struct ServeError;
impl warp::reject::Reject for ServeError {}