// src-tauri/src/rdp/mod.rs
//
// Loopback RDP Tunneling — module root.
//
// The whole point of this subsystem is the optical air-gap: we never touch VM
// memory, we never inject into the guest. We spawn mstsc.exe pointed at the
// loopback adapter (127.0.0.2), then capture the *rendered pixels on the host
// monitor*. The VM is a black box; everything we know about it comes through
// what RDP draws on our screen and what audio it redirects back to us.
//
// Pipeline:
//   client.rs  -> spawns mstsc.exe with a generated .rdp config
//   stream.rs  -> captures the mstsc window's rendered output (optical gap)
//   audio.rs   -> taps RDP audio redirection for speech-to-text
//   input.rs   -> optional synthetic input forwarding (per-profile gated)

pub mod client;
pub mod stream;
pub mod audio;
pub mod input;

pub use client::{RdpClient, RdpClientConfig};
pub use stream::{StreamCapture, Frame};
pub use audio::{AudioTap, AudioSegment};
pub use input::{InputForwarder, InputPolicy};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// Tunneled-session errors surfaced up through the orchestrator.
#[derive(Debug, Error)]
pub enum RdpError {
    #[error("mstsc.exe failed to spawn: {0}")]
    ClientSpawn(String),

    #[error("RDP handshake timed out after {0} ms — guest may not be listening on 127.0.0.2")]
    HandshakeTimeout(u64),

    #[error("stream capture lost its target window (mstsc closed or minimized to tray)")]
    StreamLost,

    #[error("audio redirection endpoint unavailable: {0}")]
    AudioUnavailable(String),

    #[error("input forwarding rejected by policy — target profile forbids synthetic input")]
    InputPolicyDenied,

    #[error("tunnel already torn down")]
    AlreadyDisconnected,
}

/// Lifecycle of a single tunnel instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    /// mstsc.exe not yet spawned.
    Idle,
    /// Client launched, waiting for the logon desktop to paint.
    Connecting,
    /// Full desktop rendered, capture loop running.
    Live,
    /// teardown() called; draining capture buffers.
    Disconnecting,
    /// Dead. Requires a new RDPTunnel to retry.
    Closed,
}

/// Global tunnel configuration — mutated per target profile before connect.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// Loopback address the VM's private vSwitch is NAT'd to.
    pub loopback_addr: String, // default "127.0.0.2"
    /// Stored guest credentials for silent connect (no credential prompt).
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    /// Host-side render resolution of the mstsc window.
    pub desktop_width: u32,
    pub desktop_height: u32,
    /// Whether audio redirection is enabled for this session.
    pub audio_redirection: bool,
    /// Input policy — some lockdown browsers fingerprint synthetic input.
    pub input_policy: InputPolicy,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            loopback_addr: "127.0.0.2".to_string(),
            username: String::new(),
            password: String::new(),
            domain: None,
            desktop_width: 1920,
            desktop_height: 1080,
            audio_redirection: true,
            input_policy: InputPolicy::Forbidden, // safe default: off
        }
    }
}

/// The public face of the rdp subsystem. Holds one live mstsc session.
///
/// The VM orchestrator constructs this after `verify_rdp_ready()` returns
/// true on the guest side, then hands the `Frame` stream to the AI engine's
/// WebSocket ingress.
pub struct RDPTunnel {
    config: TunnelConfig,
    client: RdpClient,
    stream: StreamCapture,
    audio: AudioTap,
    input: InputForwarder,
    running: Arc<AtomicBool>,
}

impl RDPTunnel {
    /// Build a tunnel from config. Does not spawn anything yet.
    pub fn new(config: TunnelConfig) -> Self {
        let running = Arc::new(AtomicBool::new(false));
        Self {
            client: RdpClient::new(&config),
            stream: StreamCapture::new(),
            audio: AudioTap::new(config.audio_redirection),
            input: InputForwarder::new(config.input_policy),
            config,
            running,
        }
    }

    /// Current lifecycle state, derived from subsystem health.
    pub fn state(&self) -> TunnelState {
        if !self.running.load(Ordering::SeqCst) {
            return if self.client.ever_connected() {
                TunnelState::Closed
            } else {
                TunnelState::Idle
            };
        }
        match self.client.is_alive() {
            true => TunnelState::Live,
            false => TunnelState::Disconnecting,
        }
    }

    /// Spawn mstsc.exe with the generated .rdp file, wait for the logon
    /// desktop to paint, then arm the optical capture loop.
    ///
    /// Blocks up to `config.timeout` (see `RdpClientConfig`) — call from a
    /// worker thread or spawn_blocking context.
    pub fn connect(&mut self) -> Result<(), RdpError> {
        if self.running.load(Ordering::SeqCst) {
            // Idempotent: a live tunnel just no-ops.
            return Ok(());
        }

        self.client
            .spawn()
            .map_err(RdpError::ClientSpawn)?;

        self.client
            .wait_for_desktop()
            .map_err(|e| RdpError::HandshakeTimeout(e.as_millis() as u64))?;

        // Attach capture *before* flipping the running flag so no frame is
        // ever missed between desktop-paint and capture-start.
        self.stream
            .attach_to_client_window(&self.client)
            .map_err(|_| RdpError::StreamLost)?;

        if self.config.audio_redirection {
            // Audio tap failure is non-fatal — visual-only sessions are
            // valid for profiles where the exam questions are on-screen text.
            let _ = self.audio.start();
        }

        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Latest captured frame from the host-side render of the mstsc window.
    /// Returns None until the first frame lands post-connect.
    pub fn latest_frame(&self) -> Option<Frame> {
        self.stream.latest()
    }

    /// Drain pending audio segments for the speech-to-text pipeline.
    pub fn drain_audio(&mut self) -> Vec<AudioSegment> {
        self.audio.drain()
    }

    /// Attempt to forward a keystroke into the RDP session.
    /// Fails closed if the target profile's InputPolicy forbids it.
    pub fn send_key(&mut self, vk: u16) -> Result<(), RdpError> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(RdpError::AlreadyDisconnected);
        }
        self.input
            .send_key(&self.client, vk)
            .map_err(|_| RdpError::InputPolicyDenied)
    }

    /// Graceful teardown: stop capture, kill mstsc, free the audio endpoint.
    /// The VM itself is destroyed separately by the vm::lifecycle module —
    /// this only concerns the host-side client window.
    pub fn teardown(&mut self) -> Result<(), RdpError> {
        if !self.running.swap(false, Ordering::SeqCst) {
            return Err(RdpError::AlreadyDisconnected);
        }
        self.stream.stop();
        self.audio.stop();
        self.input.stop();
        self.client
            .kill()
            .map_err(RdpError::ClientSpawn)?;
        Ok(())
    }
}

impl Drop for RDPTunnel {
    fn drop(&mut self) {
        // Never leak an mstsc.exe process — a stray RDP window on the host
        // monitor is exactly the artifact a proctor would notice.
        let _ = self.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_loopback_spec() {
        let c = TunnelConfig::default();
        assert_eq!(c.loopback_addr, "127.0.0.2");
        assert_eq!(c.desktop_width, 1920);
        assert_eq!(c.desktop_height, 1080);
        assert!(!matches!(c.input_policy, InputPolicy::Allowed { .. }));
    }

    #[test]
    fn teardown_before_connect_is_err() {
        let mut t = RDPTunnel::new(TunnelConfig::default());
        assert!(matches!(t.teardown(), Err(RdpError::AlreadyDisconnected)));
    }

    #[test]
    fn fresh_tunnel_is_idle() {
        let t = RDPTunnel::new(TunnelConfig::default());
        assert_eq!(t.state(), TunnelState::Idle);
    }
}