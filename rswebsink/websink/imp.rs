use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
// use gst_video::prelude::*;
// use gst_video::subclass::prelude::*;

use std::sync::Mutex;

use std::sync::LazyLock;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};

// use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use warp::Filter;
use rust_embed::RustEmbed;
use std::borrow::Cow;

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

// Structure to hold peer connection
struct PeerConnection {
    peer_id: String,
    // We'll implement WebRTC connection using webrtc-rs later
}

// Element state containing HTTP server and WebRTC components
struct State {
    // actual_port: u16,
    runtime: Option<Runtime>,
    server_handle: Option<tokio::task::JoinHandle<()>>,
    peer_connections: HashMap<String, PeerConnection>,
    unblock_tx: Option<mpsc::Sender<i32>>,
    unblock_rx: Option<mpsc::Receiver<i32>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            // actual_port: 0,
            runtime: None,
            server_handle: None,
            peer_connections: HashMap::new(),
            unblock_tx: None,
            unblock_rx: None,
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
                gst::info!(
                    CAT,
                    "Changing port from {} to {}",
                    settings.port,
                    port
                );
                settings.port = port;
            }
            "stun-server" => {
                let mut settings = self.settings.lock().unwrap();
                let stun_server = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_STUN_SERVER.to_string());
                gst::info!(
                    CAT,
                    "Changing stun-server from {} to {}",
                    settings.stun_server,
                    stun_server
                );
                settings.stun_server = stun_server;
            }
            "is-live" => {
                let mut settings = self.settings.lock().unwrap();
                let is_live = value.get::<bool>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    "Changing is-live from {} to {}",
                    settings.is_live,
                    is_live
                );
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
        gst::info!(CAT, "Starting WebSink");

        // Initialize Tokio runtime
        let runtime = match Runtime::new() {
            Ok(rt) => rt,
            Err(err) => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to create Tokio runtime: {}", err]
                ));
            }
        };

        // Setup an unblock channel for live mode
        let (tx, rx) = mpsc::channel(1);

        let mut state = self.state.lock().unwrap();
        state.runtime = Some(runtime);
        state.unblock_tx = Some(tx);
        state.unblock_rx = Some(rx);

        // let settings = self.settings.lock().unwrap().clone();
        let rt = state.runtime.as_ref().expect("Runtime should be initialized");

        let port = self.settings.lock().unwrap().port; // Use the initially configured port for this log

        // let server_handle = self.start_http_server(port, rt).map_err(|e| {
        //     gst::error_msg!(gst::ResourceError::Failed, ["Failed to start HTTP server: {}", e])
        // })?;

        let server_handle = rt.spawn(async move {

            // API session placeholder
            let api_session = warp::path!("api" / "session")
                .and(warp::post())
                .and(warp::body::json())
                .map(|_body: SessionRequest| {
                    // For now, just acknowledge. Later, this will handle WebRTC setup.
                    let response = SessionResponse {
                        answer: serde_json::json!({"type": "answer", "sdp": "dummy"}),
                        session_id: "dummy_session_id".to_string(),
                    };
                    warp::reply::json(&response)
                });

            let static_assets = warp::path::tail().and_then(|tail: warp::path::Tail| async move {
                let path = tail.as_str();
                let path_to_serve = if path.is_empty() || path == "/" {
                    "index.html"
                } else {
                    path
                };

                match Asset::get(path_to_serve) {
                    Some(content) => {
                        let mime = mime_guess::from_path(path_to_serve).first_or_octet_stream();
                        let body: Cow<'static, [u8]> = content.data;
                        let response = warp::http::Response::builder()
                            .header("Content-Type", mime.as_ref())
                            .body(body)
                            .map_err(|e| {
                                gst::error!(CAT, "Failed to build response: {}", e);
                                warp::reject::custom(ServeError)
                            })?;
                        Ok(response)
                    }
                    None => {
                        gst::debug!(CAT, "Static asset not found: {}", path_to_serve);
                        Err(warp::reject::not_found())
                    }
                }
            });

            let routes = api_session.or(static_assets);

            gst::info!(CAT, "HTTP server attempting to start on http://0.0.0.0:{}", port);
            println!("{}HTTP server configured for http://localhost:{}{}", GREEN, port, RESET);

            warp::serve(routes).run(([0, 0, 0, 0], port)).await;
            gst::info!(CAT, "HTTP server on port {} stopped.", port);
        });

        // Store the server handle in the state
        let mut locked_state = self.state.lock().unwrap(); // Re-lock to modify state
        locked_state.server_handle = Some(server_handle);

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::info!(CAT, "Stopping WebSink");

        // Clean up resources
        let mut state = self.state.lock().unwrap();

        // Stop the HTTP server
        if let Some(handle) = state.server_handle.take() {
            gst::info!(CAT, "Aborting HTTP server task...");
            handle.abort();
            // Optionally, could await the handle here if running in a context that allows it,
            // but abort is generally sufficient for cleanup.
            gst::info!(CAT, "HTTP server task aborted.");
        }

        // Clear peer connections
        state.peer_connections.clear();

        // Reset state
        state.unblock_tx = None;
        state.unblock_rx = None;
        state.runtime = None;

        // Reset peer count
        self.num_peers.store(0, Ordering::SeqCst);

        gst::info!(CAT, "WebSink stopped");
        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Get the number of connected peers
        let num_peers = self.num_peers.load(Ordering::SeqCst);
        let settings_guard = self.settings.lock().unwrap(); // Keep the guard

        // In live mode, we skip rendering if no peers are connected
        if settings_guard.is_live && num_peers == 0 {
            gst::trace!(CAT, "No peers connected, skipping buffer");
            return Ok(gst::FlowSuccess::Ok);
        }

        // For now, we'll just log the buffer information
        gst::trace!(CAT, "Rendered buffer {:?}", buffer);

        // In a real implementation, we would:
        // 1. Map the buffer data
        // 2. Send it to all WebRTC peers
        // This will be implemented later using webrtc-rs

        Ok(gst::FlowSuccess::Ok)
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, "Unlocking");

        // Signal to unblock the render method
        let state = self.state.lock().unwrap();
        if let Some(tx) = &state.unblock_tx {
            let _ = tx.try_send(-1);
        }

        Ok(())
    }
}

impl WebSink {
    fn start_http_server(&self, port:u16, rt:&Runtime) -> Result<tokio::task::JoinHandle<()>, gst::ErrorMessage> {
        // let settings = self.settings.lock().unwrap().clone();
        let state = self.state.lock().unwrap();

        let server_handle = rt.spawn(async move {
            // API session placeholder
            let api_session = warp::path!("api" / "session")
                .and(warp::post())
                .and(warp::body::json())
                .map(|_body: SessionRequest| {
                    // For now, just acknowledge. Later, this will handle WebRTC setup.
                    let response = SessionResponse {
                        answer: serde_json::json!({"type": "answer", "sdp": "dummy"}),
                        session_id: "dummy_session_id".to_string(),
                    };
                    warp::reply::json(&response)
                });

            let static_assets = warp::path::tail().and_then(|tail: warp::path::Tail| async move {
                let path = tail.as_str();
                let path_to_serve = if path.is_empty() || path == "/" {
                    "index.html"
                } else {
                    path
                };

                match Asset::get(path_to_serve) {
                    Some(content) => {
                        let mime = mime_guess::from_path(path_to_serve).first_or_octet_stream();
                        let body: Cow<'static, [u8]> = content.data;
                        let response = warp::http::Response::builder()
                            .header("Content-Type", mime.as_ref())
                            .body(body)
                            .map_err(|e| {
                                gst::error!(CAT, "Failed to build response: {}", e);
                                warp::reject::custom(ServeError)
                            })?;
                        Ok(response)
                    }
                    None => {
                        gst::debug!(CAT, "Static asset not found: {}", path_to_serve);
                        Err(warp::reject::not_found())
                    }
                }
            });

            let routes = api_session.or(static_assets);

            gst::info!(CAT, "HTTP server attempting to start on http://0.0.0.0:{}", port);
            println!("{}HTTP server configured for http://localhost:{}{}", GREEN, port, RESET);

            warp::serve(routes).run(([0, 0, 0, 0], port)).await;
            gst::info!(CAT, "HTTP server on port {} stopped.", port);
        });
        Ok(server_handle)
    }
}

#[derive(Debug)]
struct ServeError;
impl warp::reject::Reject for ServeError {}