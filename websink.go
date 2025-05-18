// a webrtc video to browser sink-element implemented in Go-gst.
//
// The websink plugin accepts H264 video input and streams it to web browsers
// using WebRTC. It sets up an HTTP server to serve the client webpage and
// manages WebRTC connections for multiple clients.
//
// +plugin:Name=websink
// +plugin:Description=WebRTC h264 sink
// +plugin:Version=v0.1.0
// +plugin:Author=videologyinc
// +plugin:License=gst.LicenseMIT
// +plugin:Source=websink
// +plugin:Package=websink
// +plugin:Origin=https://github.com/videologyinc/websink
// +plugin:ReleaseDate=2025-03-18
//
// +element:Name=websink
// +element:Rank=gst.RankNone
// +element:Impl=WebSink
// +element:Subclass=base.ExtendsBaseSink
//
//go:generate gst-plugin-gen

package main

import (
	"embed"
	"encoding/json"
	"fmt"
	"io"
	"io/fs"
	"net"
	"net/http"
	"os"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

	"github.com/go-gst/go-glib/glib"
	"github.com/go-gst/go-gst/gst"
	"github.com/go-gst/go-gst/gst/base"
	"github.com/pion/webrtc/v4"
	"github.com/pion/webrtc/v4/pkg/media"
)

// defaults:
var (
	DefaultPort       = 8091
	DefaultStunServer = "stun:stun.l.google.com:19302"
	// print colors
	GREEN = "\033[32m"
	RED   = "\033[31m"
	RESET = "\033[0m"
)

// main is left unimplemented since these files are compiled to c-shared.
func main() {}

// CAT is the log category for the websink
var CAT = gst.NewDebugCategory(
	"websink",
	gst.DebugColorNone,
	"Websink Element",
)

// SessionRequest represents the JSON structure for session requests
type SessionRequest struct {
	Offer json.RawMessage `json:"offer"`
}

// SessionResponse represents the JSON structure for session responses
type SessionResponse struct {
	Answer    json.RawMessage `json:"answer"`
	SessionId string          `json:"sessionId"`
}

// Here we define a list of ParamSpecs that will make up the properties for our element.
var properties = []*glib.ParamSpec{
	glib.NewIntParam(
		"port", "HTTP Port", "Port to use for the HTTP server (0 for auto)",
		0, 65535, DefaultPort,
		glib.ParameterReadWrite,
	),
	glib.NewStringParam(
		"stun-server", "STUN Server", "STUN server to use for WebRTC (empty for none)",
		&DefaultStunServer,
		glib.ParameterReadWrite,
	),
	glib.NewBoolParam(
		"is-live", "Live Mode", "Whether to block Render without peers (default: false)",
		false,
		glib.ParameterReadWrite,
	),
}

// Here we declare a private struct to hold our internal state.
type state struct {
	// Whether the element is started or not
	started bool
	// The HTTP server
	server *http.Server
	// The actual port being used
	actualPort int
	// The WebRTC configuration
	webrtcConfig webrtc.Configuration
	// Map to store active peer connections
	peerConnectionsMutex sync.RWMutex
	peerConnections      map[string]*webrtc.PeerConnection
	numPeers             atomic.Int32
	// Channel to notify about peer connection changes
	unblock chan int32
	// Shared video track
	videoTrack *webrtc.TrackLocalStaticSample
}

// This is another private struct where we hold the parameter values set on our element.
type settings struct {
	port       int
	stunServer string
	isLive     bool
}

//go:embed static/*
var staticFiles embed.FS

// WebSink is our implementation of a GStreamer sink element
type WebSink struct {
	// The settings for the element
	settings *settings
	// The current state of the element
	state *state
}

// updatePeerConnections adds or removes a peer connection from the global map
// and prints the current count
func (w *WebSink) updatePeerConnections(id string, pc *webrtc.PeerConnection, add bool) {
	w.state.peerConnectionsMutex.Lock()
	if add {
		w.state.peerConnections[id] = pc
	} else {
		delete(w.state.peerConnections, id)
	}
	w.state.numPeers.Store(int32(len(w.state.peerConnections)))
	w.state.peerConnectionsMutex.Unlock()
	CAT.Log(gst.LevelInfo, fmt.Sprintf("Client count changed: %d connected clients", w.state.numPeers.Load()))

	// Send notification on peer change channel (non-blocking)
	select {
	case w.state.unblock <- w.state.numPeers.Load():
	default:
	}
}

// findAvailablePort finds an available port starting from the given port
func findAvailablePort(startPort int) (int, error) {
	// If startPort is 0, find any available port
	if startPort == 0 {
		listener, err := net.Listen("tcp", ":0")
		if err != nil {
			return 0, err
		}
		port := listener.Addr().(*net.TCPAddr).Port
		listener.Close()
		return port, nil
	}

	// Otherwise, try the specified port and increment if not available
	port := startPort
	maxPort := startPort + 100 // Try up to 100 ports

	for port < maxPort {
		addr := fmt.Sprintf(":%d", port)
		listener, err := net.Listen("tcp", addr)
		if err == nil {
			listener.Close()
			return port, nil
		}
		port++
	}

	return 0, fmt.Errorf("no available ports found between %d and %d", startPort, maxPort)
}

// handleSession creates a handler for the /api/session endpoint
func (w *WebSink) handleSession(resp http.ResponseWriter, req *http.Request) {
	if req.Method != http.MethodPost {
		http.Error(resp, "Method not allowed", http.StatusMethodNotAllowed)
		return
	}

	// Read the request body
	body, err := io.ReadAll(req.Body)
	if err != nil {
		http.Error(resp, "Error reading request body", http.StatusBadRequest)
		return
	}

	// Parse the JSON request
	var sessionReq SessionRequest
	if err := json.Unmarshal(body, &sessionReq); err != nil {
		http.Error(resp, "Error parsing JSON", http.StatusBadRequest)
		return
	}

	// Generate a unique ID for this peer connection
	peerID := fmt.Sprintf("peer-%d", len(w.state.peerConnections)+1)

	// Create a new peer connection for this client
	peerConnection, err := w.createPeerConnection(peerID)
	if err != nil {
		http.Error(resp, "Error creating peer connection: "+err.Error(), http.StatusInternalServerError)
		return
	}

	// Add to the peer connections map
	w.updatePeerConnections(peerID, peerConnection, true)

	// Decode the offer
	offer := webrtc.SessionDescription{}
	if err := json.Unmarshal(sessionReq.Offer, &offer); err != nil {
		http.Error(resp, "Error parsing offer: "+err.Error(), http.StatusBadRequest)
		// Remove from peer connections map if we fail
		w.updatePeerConnections(peerID, nil, false)
		return
	}

	// Set the remote SessionDescription
	if err := peerConnection.SetRemoteDescription(offer); err != nil {
		http.Error(resp, "Error setting remote description: "+err.Error(), http.StatusInternalServerError)
		// Remove from peer connections map if we fail
		w.updatePeerConnections(peerID, nil, false)
		return
	}

	// Create an answer
	answer, err := peerConnection.CreateAnswer(nil)
	if err != nil {
		http.Error(resp, "Error creating answer: "+err.Error(), http.StatusInternalServerError)
		// Remove from peer connections map if we fail
		w.updatePeerConnections(peerID, nil, false)
		return
	}

	// Sets the LocalDescription, and starts our UDP listeners
	if err = peerConnection.SetLocalDescription(answer); err != nil {
		http.Error(resp, "Error setting local description: "+err.Error(), http.StatusInternalServerError)
		// Remove from peer connections map if we fail
		w.updatePeerConnections(peerID, nil, false)
		return
	}

	// Wait for ICE gathering to complete
	gatherComplete := webrtc.GatheringCompletePromise(peerConnection)
	<-gatherComplete

	// Marshal the answer to JSON
	answerJSON, err := json.Marshal(peerConnection.LocalDescription())
	if err != nil {
		http.Error(resp, "Error encoding answer: "+err.Error(), http.StatusInternalServerError)
		// Remove from peer connections map if we fail
		w.updatePeerConnections(peerID, nil, false)
		return
	}

	// Return the answer as JSON
	resp.Header().Set("Content-Type", "application/json")
	response := SessionResponse{
		Answer:    answerJSON,
		SessionId: peerID,
	}
	json.NewEncoder(resp).Encode(response)
}

// createPeerConnection creates a new peer connection with the shared tracks
func (w *WebSink) createPeerConnection(peerID string) (*webrtc.PeerConnection, error) {
	// Create a new RTCPeerConnection
	peerConnection, err := webrtc.NewPeerConnection(w.state.webrtcConfig)
	if err != nil {
		return nil, err
	}

	// Set the handler for ICE connection state
	// This will notify you when the peer has connected/disconnected
	peerConnection.OnICEConnectionStateChange(func(connectionState webrtc.ICEConnectionState) {
		CAT.Log(gst.LevelInfo, fmt.Sprintf("Connection State for %s has changed to %s", peerID, connectionState.String()))

		// Clean up when disconnected
		if connectionState == webrtc.ICEConnectionStateDisconnected ||
			connectionState == webrtc.ICEConnectionStateFailed ||
			connectionState == webrtc.ICEConnectionStateClosed {
			CAT.Log(gst.LevelInfo, fmt.Sprintf("Peer %s disconnected, cleaning up", peerID))
			// Remove from peer connections map
			w.updatePeerConnections(peerID, nil, false)
			// Close the peer connection to free resources
			peerConnection.Close()
		}
	})

	// Add the video track to the peer connection
	_, err = peerConnection.AddTrack(w.state.videoTrack)
	if err != nil {
		return nil, err
	}

	return peerConnection, nil
}

// startHTTPServer starts the HTTP server for the websink
func (w *WebSink) startHTTPServer(self *base.GstBaseSink) bool {
	// Find an available port
	port, err := findAvailablePort(w.settings.port)
	if err != nil {
		self.Log(CAT, gst.LevelError, "Could not find available port")
		return false
	}
	w.state.actualPort = port

	// Set up HTTP handlers
	mux := http.NewServeMux()
	// fileserver := http.FileServer(http.Dir("./static"))
	static, _ := fs.Sub(staticFiles, "static")
	fileserver := http.FileServer(http.FS(static))

	mux.HandleFunc("POST /api/session", w.handleSession)
	mux.Handle("GET /favicon.ico", fileserver)
	mux.Handle("GET /", fileserver)

	// Create the HTTP server
	w.state.server = &http.Server{
		Addr:    ":" + strconv.Itoa(port),
		Handler: mux,
	}

	// Start the HTTP server in a goroutine
	//get hostnames
	hostname, _ := os.Hostname()
	// get IP addr of main interface
	addr := externalIP()
	portStr := strconv.Itoa(port)

	go func() {
		if err := w.state.server.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			CAT.LogError("HTTP server error: " + err.Error())
		}
	}()

	fmt.Println(GREEN + "HTTP server started at http://" + hostname + ".local:" + portStr + " and http://" + addr + ":" + portStr + RESET)
	return true
}

// The ObjectSubclass implementations below are for registering the various aspects of our
// element and its capabilities with the type system.

// New creates a new WebSink instance
func (w *WebSink) New() glib.GoObjectSubclass {
	CAT.Log(gst.LevelLog, "Initializing new WebSink object")
	return &WebSink{
		settings: &settings{
			port:       8091,
			stunServer: "stun:stun.l.google.com:19302",
			isLive:     false,
		},
		state: &state{
			peerConnections: make(map[string]*webrtc.PeerConnection),
			unblock:         make(chan int32, 1),
		},
	}
}

// ClassInit initializes the WebSink class
func (w *WebSink) ClassInit(klass *glib.ObjectClass) {
	CAT.Log(gst.LevelLog, "Initializing websink class")
	class := gst.ToElementClass(klass)
	class.SetMetadata(
		"WebRTC Sink",
		"Sink/Network",
		"Stream H264 video to web browsers using WebRTC",
		"Go-GST Contributors",
	)
	CAT.Log(gst.LevelLog, "Adding sink pad template and properties to class")
	class.AddPadTemplate(gst.NewPadTemplate(
		"sink",
		gst.PadDirectionSink,
		gst.PadPresenceAlways,
		gst.NewCapsFromString("video/x-h264,stream-format=byte-stream,alignment=au"),
	))
	class.InstallProperties(properties)
}

// SetProperty sets a property on the WebSink
func (w *WebSink) SetProperty(self *glib.Object, id uint, value *glib.Value) {
	param := properties[id]
	switch param.Name() {
	case "port":
		if w.state.started {
			gst.ToElement(self).ErrorMessage(gst.DomainLibrary, gst.LibraryErrorSettings, "Cannot change port while WebSink is running", "")
			return
		}
		if value != nil {
			val, _ := value.GoValue()
			if val == nil {
				gst.ToElement(self).ErrorMessage(gst.DomainLibrary, gst.LibraryErrorSettings, "Invalid port number", "")
				return
			}
			intval, _ := val.(int)
			if intval < 0 || intval > 65535 {
				gst.ToElement(self).ErrorMessage(gst.DomainLibrary, gst.LibraryErrorSettings, fmt.Sprintf("Invalid port number: %d", intval), "")
				return
			}
			w.settings.port = intval
			gst.ToElement(self).Log(CAT, gst.LevelInfo, fmt.Sprintf("Set `port` to %d", intval))
		}
	case "stun-server":
		if w.state.started {
			gst.ToElement(self).ErrorMessage(gst.DomainLibrary, gst.LibraryErrorSettings, "Cannot change STUN server while WebSink is running", "")
			return
		}
		if value != nil {
			val, _ := value.GetString()
			w.settings.stunServer = val
			gst.ToElement(self).Log(CAT, gst.LevelInfo, fmt.Sprintf("Set `stun-server` to %s", val))
		}
	case "is-live":
		if value != nil {
			val, _ := value.GoValue()
			if val == nil {
				gst.ToElement(self).ErrorMessage(gst.DomainLibrary, gst.LibraryErrorSettings, "Invalid is-live value", "")
				return
			}
			boolval, _ := val.(bool)
			w.settings.isLive = boolval
			gst.ToElement(self).Log(CAT, gst.LevelInfo, fmt.Sprintf("Set `is-live` to %v", boolval))
		}
	}
}

// GetProperty gets a property from the WebSink
func (w *WebSink) GetProperty(self *glib.Object, id uint) *glib.Value {
	param := properties[id]
	switch param.Name() {
	case "port":
		val, err := glib.GValue(w.settings.port)
		if err == nil {
			return val
		}
		gst.ToElement(self).ErrorMessage(gst.DomainLibrary, gst.LibraryErrorFailed,
			fmt.Sprintf("Could not convert %d to GValue", w.settings.port),
			err.Error(),
		)
	case "stun-server":
		val, err := glib.GValue(w.settings.stunServer)
		if err == nil {
			return val
		}
		gst.ToElement(self).ErrorMessage(gst.DomainLibrary, gst.LibraryErrorFailed,
			fmt.Sprintf("Could not convert %s to GValue", w.settings.stunServer),
			err.Error(),
		)
	case "is-live":
		val, err := glib.GValue(w.settings.isLive)
		if err == nil {
			return val
		}
		gst.ToElement(self).ErrorMessage(gst.DomainLibrary, gst.LibraryErrorFailed,
			fmt.Sprintf("Could not convert %v to GValue", w.settings.isLive),
			err.Error(),
		)
	}
	return nil
}

// Start is called to start the websink
func (w *WebSink) Start(self *base.GstBaseSink) bool {
	if w.state.started {
		self.Log(CAT, gst.LevelError, "Websink is already started")
		return false
	}

	// Configure WebRTC
	w.state.webrtcConfig = webrtc.Configuration{}
	if w.settings.stunServer != "" {
		w.state.webrtcConfig.ICEServers = []webrtc.ICEServer{
			{
				URLs: []string{w.settings.stunServer},
			},
		}
	}

	// Create shared video track
	var err error
	w.state.videoTrack, err = webrtc.NewTrackLocalStaticSample(
		webrtc.RTPCodecCapability{MimeType: "video/h264"},
		"video",
		"websink",
	)
	if err != nil {
		self.Log(CAT, gst.LevelError, "Failed to create video track")
		return false
	}

	// Start HTTP server
	if !w.startHTTPServer(self) {
		return false
	}

	w.state.started = true
	self.Log(CAT, gst.LevelInfo, "Websink has started")
	return true
}

// Stop is called to stop the websink
func (w *WebSink) Stop(self *base.GstBaseSink) bool {
	if !w.state.started {
		self.Log(CAT, gst.LevelError, "Websink is not started")
		return false
	}

	// Close all peer connections
	w.state.peerConnectionsMutex.Lock()
	for id, pc := range w.state.peerConnections {
		pc.Close()
		delete(w.state.peerConnections, id)
	}
	w.state.peerConnectionsMutex.Unlock()

	// Shutdown HTTP server
	if w.state.server != nil {
		if err := w.state.server.Close(); err != nil {
			self.Log(CAT, gst.LevelError, "Failed to close HTTP server")
			return false
		}
	}

	w.state.started = false
	self.Log(CAT, gst.LevelInfo, "Websink has stopped")
	return true
}

// Render is called when a buffer is ready to be processed
func (w *WebSink) Render(self *base.GstBaseSink, buffer *gst.Buffer) gst.FlowReturn {
	if !w.state.started {
		self.Log(CAT, gst.LevelError, "Websink is not started")
		return gst.FlowError
	}

	if w.settings.isLive {
		if w.state.numPeers.Load() == 0 {
			self.Log(CAT, gst.LevelDebug, "No clients connected")
			return gst.FlowOK
		}
	} else {
		for w.state.numPeers.Load() == 0 {
			// Block until we receive a peer change notification
			self.Log(CAT, gst.LevelInfo, "Blocking until Peers connected")
			select {
			case <-w.state.unblock:
				self.Log(CAT, gst.LevelInfo, "Unblocked")
			case <-time.After(1 * time.Second):
				self.Log(CAT, gst.LevelInfo, "Timeout")
				return gst.FlowFlushing
			}
		}
	}

	samples := buffer.Map(gst.MapRead).Bytes()
	defer buffer.Unmap()

	if err := w.state.videoTrack.WriteSample(media.Sample{Data: samples, Duration: *buffer.Duration().AsDuration()}); err != nil {
		self.Log(CAT, gst.LevelError, "Error writing sample to track")
		return gst.FlowError
	}
	return gst.FlowOK
}

// Unlock informs the Render function to stop blocking.
func (w *WebSink) Unlock(self *base.GstBaseSink) bool {
	self.Log(CAT, gst.LevelInfo, "Websink Unlock")
	// Send notification on peer change channel (non-blocking)
	select {
	case w.state.unblock <- 1:
	default:
	}
	return true
}

func (w *WebSink) ChangeState(self *gst.Element, transition gst.StateChange) (ret gst.StateChangeReturn) {
	self.Log(CAT, gst.LevelTrace, fmt.Sprintf("Changing state: %s", transition))
	// Apply the transition to the parent element
	ret = self.ParentChangeState(transition)
	self.Log(CAT, gst.LevelTrace, fmt.Sprintf("Changing state: %s -> %s", transition, ret))
	return
}

func externalIP() string {
	ifaces, err := net.Interfaces()
	if err != nil {
		return "localhost"
	}
	for _, iface := range ifaces {
		if iface.Flags&net.FlagUp == 0 {
			continue // interface down
		}
		if iface.Flags&net.FlagLoopback != 0 {
			continue // loopback interface
		}
		addrs, err := iface.Addrs()
		if err != nil {
			return "localhost"
		}
		for _, addr := range addrs {
			var ip net.IP
			switch v := addr.(type) {
			case *net.IPNet:
				ip = v.IP
			case *net.IPAddr:
				ip = v.IP
			}
			if ip == nil || ip.IsLoopback() {
				continue
			}
			ip = ip.To4()
			if ip == nil {
				continue // not an ipv4 address
			}
			return ip.String()
		}
	}
	return "localhost"
}
