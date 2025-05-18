use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tower_http::services::ServeDir;
use uuid::Uuid;

use once_cell::sync::Lazy;
use gst;

// Static debug category
static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "websink-server",
        gst::DebugColorFlags::empty(),
        Some("WebRTC Sink HTTP Server"),
    )
});

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

// Event types for communication between HTTP server and WebSink element
#[derive(Debug)]
pub enum ServerEvent {
    NewSession {
        peer_id: String,
        offer: serde_json::Value,
        responder: mpsc::Sender<Result<SessionResponse, String>>,
    },
}

// Shared state between server and WebSink element
#[derive(Clone)]
pub struct ServerState {
    pub event_sender: mpsc::Sender<ServerEvent>,
}

// Find available port starting from the given port
pub async fn find_available_port(start_port: u16) -> Option<u16> {
    // If start_port is 0, find any available port
    if start_port == 0 {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.ok()?;
        return Some(listener.local_addr().ok()?.port());
    }

    // Otherwise try the specific port and up to 100 more
    let max_port = start_port.saturating_add(100);

    for port in start_port..=max_port {
        if let Ok(listener) = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            return Some(listener.local_addr().ok()?.port());
        }
    }

    None
}

// Start the server
pub async fn start_http_server(
    port: u16,
    event_sender: mpsc::Sender<ServerEvent>,
    static_dir: PathBuf,
) -> Result<(u16, tokio::task::JoinHandle<()>), anyhow::Error> {
    // Find available port
    let actual_port = match find_available_port(port).await {
        Some(p) => p,
        None => return Err(anyhow::anyhow!("No available ports found")),
    };

    // Create state
    let server_state = ServerState { event_sender };

    // Build the router
    let app = Router::new()
        .route("/api/session", post(handle_session))
        .nest_service("/", ServeDir::new(static_dir))
        .with_state(server_state);

    // Bind the server to the port
    let addr = SocketAddr::from(([0, 0, 0, 0], actual_port));

    gst::info!(
        CAT,
        "Starting HTTP server on port {}",
        actual_port
    );

    // Start the server
    let server = axum::Server::bind(&addr)
        .serve(app.into_make_service());

    // Print the IP address information
    if let Ok(hostname) = std::env::var("HOSTNAME") {
        gst::info!(CAT, "HTTP server accessible at http://{}:{}", hostname, actual_port);
    }

    // Try to get the external IP
    match local_ip_address::local_ip() {
        Ok(ip) => {
            gst::info!(CAT, "HTTP server accessible at http://{}:{}", ip, actual_port);
        }
        Err(_) => {
            gst::info!(CAT, "HTTP server accessible at http://localhost:{}", actual_port);
        }
    }

    // Spawn the server as a separate task
    let handle = tokio::spawn(async move {
        if let Err(e) = server.await {
            gst::error!(CAT, "HTTP server error: {}", e);
        }
    });

    Ok((actual_port, handle))
}

// Handle session requests (WebRTC signaling)
async fn handle_session(
    State(state): State<ServerState>,
    Json(session_req): Json<SessionRequest>,
) -> impl IntoResponse {
    gst::debug!(CAT, "Received session request");

    // Generate a unique ID for this peer connection
    let peer_id = Uuid::new_v4().to_string();

    // Create a channel for the response
    let (tx, mut rx) = mpsc::channel::<Result<SessionResponse, String>>(1);

    // Send the event to the WebSink element
    if let Err(err) = state.event_sender.send(ServerEvent::NewSession {
        peer_id: peer_id.clone(),
        offer: session_req.offer,
        responder: tx,
    }).await {
        gst::error!(CAT, "Error sending new session event: {}", err);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": "Server error"
        }))).into_response();
    }

    // Wait for the response
    match rx.recv().await {
        Some(Ok(response)) => {
            (StatusCode::OK, Json(response)).into_response()
        }
        Some(Err(err)) => {
            gst::error!(CAT, "Error creating session: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": err
            }))).into_response()
        }
        None => {
            gst::error!(CAT, "No response received from WebSink");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": "No response from server"
            }))).into_response()
        }
    }
}