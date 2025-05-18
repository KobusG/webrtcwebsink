/* eslint-env browser */

let pc = new RTCPeerConnection({
  iceServers: [
    {
      urls: 'stun:stun.l.google.com:19302'
    }
  ]
})

// Update connection status display
function showStatus(status) {
  const statusElement = document.getElementById('connectionStatus');
  statusElement.textContent = status;
}

pc.ontrack = function (event) {
  if (event.track.kind === 'video') {
    var videoElement = document.createElement('video');
    videoElement.srcObject = event.streams[0];
    videoElement.autoplay = true;
    videoElement.playsInline = true;
    videoElement.muted = true;

    // Add loadedmetadata event listener to ensure video is ready
    videoElement.addEventListener('loadedmetadata', function() {
      // Ensure playback starts
      videoElement.play().catch(e => {
        console.warn(`Autoplay failed: ${e.message}. User interaction may be required.`);
      });
    });

    // Clear any existing video elements
    const videoContainer = document.getElementById('videoContainer');
    videoContainer.innerHTML = '';
    videoContainer.appendChild(videoElement);
  }
}

let iceTimout = null

function sendOffer() {
  console.log('Sending offer...');

  // Send the offer to the server as JSON directly
  fetch('/api/session', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      offer: pc.localDescription
    }),
  })
  .then(response => {
    if (!response.ok) {
      throw new Error(`Server responded with ${response.status}: ${response.statusText}`)
    }
    return response.json()
  })
  .then(data => {
    console.log('Received answer');

    // Set the remote description with the answer from the server
    pc.setRemoteDescription(data.answer);
  })
  .catch(error => {
    console.error(`Connection failed: ${error.message}`);
  });
}

// Function to close the connection gracefully
function closeConnection() {
  console.log('Closing connection...');

  // Close the RTCPeerConnection
  // This will send the appropriate signals to the server
  if (pc && pc.connectionState !== 'closed') {
    // Close the peer connection
    pc.close();
    showStatus('Disconnected');
    console.log('Connection closed');
  }
}

pc.oniceconnectionstatechange = e => {
  const state = pc.iceConnectionState;
  console.log(`Connection: ${state}`);

  // When connection is established, try to play all video elements
  if (state === 'connected' || state === 'completed') {
    showStatus('Connected');
    // Find all video elements and try to play them
    document.querySelectorAll('video').forEach(el => {
      el.play().catch(e => {
        console.warn(`Autoplay failed: ${e.message}`);
      });
      showStatus('Playing');
    });
  }
}

pc.onicecandidate = event => {
  if (event.candidate === null) {
    // ICE gathering is complete, we can now send the offer to the server
    console.log('ICE gathering complete');
  } else {
    // fire after 150ms of no new candidates
    if (iceTimout) {
      clearTimeout(iceTimout)
    }
    iceTimout = setTimeout(sendOffer, 150)
  }
}

// Initialize connection
console.log('Starting connection...');

// Offer to receive video track only
pc.addTransceiver('video', {'direction': 'sendrecv'})
pc.createOffer()
   .then(offer => {
       console.log('Offer created');
       return pc.setLocalDescription(offer);
   })
   .catch(error => {
       console.log('Error creating offer');
       console.error(`Error: ${error.message}`);
   });

// Add event listener for page unload
window.addEventListener('beforeunload', function(event) {
  closeConnection();
});

// Add event listener for page visibility change
// document.addEventListener('visibilitychange', function() {
//   if (document.visibilityState === 'hidden') {
//     // Page is hidden (tab switched, minimized, etc.)
//     // This can help in some cases where beforeunload doesn't fire
//     closeConnection();
//   }
// });
