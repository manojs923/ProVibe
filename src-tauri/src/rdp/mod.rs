// src-tauri/src/rdp/mod.rs
//
// Loopback RDP Tunneling — module root.
//
// We spawn mstsc.exe pointed at the loopback adapter (127.0.0.2), then capture
// the *rendered pixels on the host monitor*. Nothing touches guest memory.
//
// Pipeline:
//   client.rs  -> spawns mstsc.exe with a generated .rdp config
//   stream.rs  -> captures the mstsc window's rendered output
//   audio.rs   -> taps RDP audio redirection for speech-to-text
//   input.rs   -> optional synthetic input forwarding

pub mod audio;
pub mod client;
pub mod input;
pub mod stream;

use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use crate::rdp::audio::AudioTap;
use crate::rdp::client::{ClientError, RdpClient};
use crate::rdp::input::{InputConfig, InputError, InputEvent, InputForwarder};
use crate::rdp::stream::{CaptureConfig, Frame, StreamCapture, StreamError};

// -----------------------------------------------------------------------------
// Contracts expected from sibling modules
// -----------------------------------------------------------------------------
//
// client.rs:
//   pub struct RdpClient; pub struct RDPClientHandle (Clone, Send + Sync);
//   pub enum ClientError (thiserror, includes Spawn(String), Timeout(Duration),
//       NotRunning, InvalidPath(String), StageCredentials(String), ConfigIo(String));
//   impl RdpClient {
//       pub fn new(cfg: &TunnelConfig) -> Self;
//       pub fn spawn(&mut self) -> Result<(), ClientError>;
//       pub fn wait_for_desktop(&mut self) -> Result<(), ClientError>;
//       pub fn is_alive(&mut self) -> bool;
//       pub fn ever_connected(&self) -> bool;
//       pub fn handle(&self) -> Option<RDPClientHandle>;
//       pub async fn disconnect(&mut self) -> Result<(), ClientError>;
//   }
//   impl RDPClientHandle {
//       pub fn pid(&self) -> u32;
//       pub fn window_title(&self) -> String;
//       pub fn get_window_handle(&self) -> Result<HWND, windows::core::Error>;
//       pub fn get_guest_resolution(&self) -> (u32, u32);
//   }
//
// stream.rs:
//   pub struct StreamCapture; pub struct CaptureConfig (Clone); pub enum StreamError;
//   impl StreamCapture {
//       pub async fn attach(handle: &RDPClientHandle, cfg: CaptureConfig)
//           -> Result<Self, StreamError>;
//       pub async fn next_frame(&mut self) -> Result<Frame, StreamError>;
//       pub async fn detach(self) -> Result<(), StreamError>;
//   }
//
// audio.rs:
//   impl AudioTap {
//       pub fn new(enabled: bool) -> Self;
//       pub fn start(&mut self, handle: &RDPClientHandle) -> Result<(), String>;
//       pub fn stop(&mut self) -> Result<(), String>;
//   }
//
// input.rs:
//   impl InputForwarder {
//       pub fn new(handle: &RDPClientHandle, cfg: InputConfig) -> Result<Self, InputError>;
//       pub fn send(&mut self, event: InputEvent) -> Result<(), InputError>;
//       pub fn shutdown(&mut self) -> Result<(), InputError>;
//   }
// -----------------------------------------------------------------------------

// =============================================================================
// Tauri commands
// =============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct TunnelStatus {
    pub active: bool,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub latency_ms: Option<u64>,
    pub adapter_name: String,
}

#[tauri::command]
pub fn get_tunnel_status(local_port: Option<u16>) -> Result<TunnelStatus, String> {
    let port = local_port.unwrap_or(3389);

    let sock_addr: std::net::SocketAddr = format!("127.0.0.1:{}", port)
        .parse()
        .map_err(|e: std::net::AddrParseError| e.to_string())?;

    // Check if something is listening on the RDP port.
    let listener_active =
        TcpStream::connect_timeout(&sock_addr, Duration::from_millis(500)).is_ok();

    // Check loopback adapter existence.
    let adapter_output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-NetAdapter | Where-Object { $_.Name -like '*Loopback*' -or $_.Name -like '*RDP*' -or $_.Name -like '*Tunnel*' } | Select-Object Name, Status | ConvertTo-Json",
        ])
        .output()
        .map_err(|e| format!("Adapter check failed: {}", e))?;

    let adapter_info = String::from_utf8_lossy(&adapter_output.stdout);
    let adapter_name = if adapter_info.contains("Up") {
        adapter_info
            .lines()
            .find(|l| l.contains("Name"))
            .and_then(|l| l.split(':').nth(1))
            .map(|s| s.trim().trim_matches(|c| c == '"' || c == ',').to_string())
            .unwrap_or_else(|| "Unknown".to_string())
    } else {
        "None".to_string()
    };

    // Measure latency if active.
    let latency = if listener_active {
        let start = std::time::Instant::now();
        if TcpStream::connect_timeout(&sock_addr, Duration::from_secs(1)).is_ok() {
            Some(start.elapsed().as_millis() as u64)
        } else {
            None
        }
    } else {
        None
    };

    Ok(TunnelStatus {
        active: listener_active,
        local_port: port,
        remote_host: "127.0.0.1".to_string(),
        remote_port: port,
        latency_ms: latency,
        adapter_name,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelStartResult {
    pub success: bool,
    pub adapter_name: String,
    pub ip_address: String,
    pub message: String,
}

#[tauri::command]
pub fn start_loopback_tunnel_cmd(
    adapter_name: Option<String>,
    ip_address: Option<String>,
) -> Result<TunnelStartResult, String> {
    let adapter = adapter_name.unwrap_or_else(|| "ProtocolLoopback".to_string());
    let ip = ip_address.unwrap_or_else(|| "192.168.137.1".to_string());

    let install_script = format!(
        r#"
        $existing = Get-NetAdapter -Name '{adapter}' -ErrorAction SilentlyContinue
        if (-not $existing) {{
            $devconPath = "C:\Program Files (x86)\Windows Kits\10\Tools\x64\devcon.exe"
            if (Test-Path $devconPath) {{
                & $devconPath install $env:WINDIR\INF\netloop.inf *MSLOOP
            }} else {{
                Install-WindowsFeature -Name RSAT-Loopback -ErrorAction SilentlyContinue
            }}
            Start-Sleep -Seconds 2
            $adapters = Get-NetAdapter | Where-Object {{ $_.InterfaceDescription -like '*Loopback*' }} | Sort-Object ifIndex | Select-Object -Last 1
            if ($adapters) {{ Rename-NetAdapter -Name $adapters.Name -NewName '{adapter}' }}
        }}
        Enable-NetAdapter -Name '{adapter}' -Confirm:$false
        "#,
        adapter = adapter
    );

    Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &install_script])
        .output()
        .map_err(|e| format!("Adapter installation failed: {}", e))?;

    let ip_script = format!(
        r#"
        $iface = Get-NetAdapter -Name '{adapter}'
        $iface | New-NetIPAddress -IPAddress {ip} -PrefixLength 24 -ErrorAction SilentlyContinue
        $iface | Set-NetIPInterface -InterfaceMetric 5
        "#,
        adapter = adapter,
        ip = ip
    );

    let ip_output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ip_script])
        .output()
        .map_err(|e| format!("IP assignment failed: {}", e))?;

    let success = ip_output.status.success();

    Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Set-ItemProperty -Path 'HKLM:\\System\\CurrentControlSet\\Control\\Terminal Server' -Name 'fDenyTSConnections' -Value 0; Enable-NetFirewallRule -DisplayGroup 'Remote Desktop'",
        ])
        .output()
        .map_err(|e| format!("RDP enable failed: {}", e))?;

    Ok(TunnelStartResult {
        success,
        adapter_name: adapter,
        ip_address: ip,
        message: if success {
            "Loopback tunnel established".to_string()
        } else {
            "Adapter created but IP assignment may have failed".to_string()
        },
    })
}

#[tauri::command]
pub fn stop_loopback_tunnel(adapter_name: Option<String>) -> Result<String, String> {
    let adapter = adapter_name.unwrap_or_else(|| "ProtocolLoopback".to_string());

    let script = format!(
        r#"
        $adapter = Get-NetAdapter -Name '{0}' -ErrorAction SilentlyContinue
        if ($adapter) {{
            Disable-NetAdapter -Name '{0}' -Confirm:$false
            Write-Output "Disabled"
        }} else {{
            Write-Output "NotFound"
        }}
        "#,
        adapter
    );

    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .map_err(|e| format!("Stop tunnel failed: {}", e))?;

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    match result.as_str() {
        "Disabled" => Ok(format!("Tunnel adapter '{}' disabled", adapter)),
        "NotFound" => Ok("No active tunnel found".to_string()),
        _ => Err(format!("Unexpected result: {}", result)),
    }
}

// =============================================================================
// Errors
// =============================================================================

#[derive(Debug, Error)]
pub enum RdpError {
    #[error("Client error: {0}")]
    Client(#[from] ClientError),

    #[error("Stream error: {0}")]
    Stream(#[from] StreamError),

    #[error("Input error: {0}")]
    Input(#[from] InputError),

    #[error("Audio error: {0}")]
    Audio(String),

    #[error("Not connected")]
    NotConnected,

    #[error("Already initialized: {0}")]
    AlreadyInitialized(&'static str),

    #[error("Invalid PID (0 is not a valid process identifier)")]
    InvalidPid,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// =============================================================================
// Config & state
// =============================================================================

/// Lifecycle of a single tunnel instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    /// mstsc.exe not yet spawned.
    Idle,
    /// Client launched, waiting for the logon desktop to paint.
    Connecting,
    /// Full desktop rendered, capture loop running.
    Live,
    /// shutdown() in progress.
    Disconnecting,
    /// Dead. Build a new Tunnel to retry.
    Closed,
}

/// Tunnel configuration — mutated per target profile before connect.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    pub loopback_addr: String, // default "127.0.0.2"
    pub username: String,
    pub password: String,
    pub domain: String,
    pub desktop_width: u32,
    pub desktop_height: u32,
    pub color_depth: u32,
    pub audio_redirection: bool,
    pub capture: CaptureConfig,
    pub input: InputConfig,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            loopback_addr: "127.0.0.2".to_string(),
            username: String::new(),
            password: String::new(),
            domain: String::new(),
            desktop_width: 1920,
            desktop_height: 1080,
            color_depth: 32,
            audio_redirection: true,
            capture: CaptureConfig::default(),
            input: InputConfig::default(),
        }
    }
}

// =============================================================================
// Tunnel
// =============================================================================

pub struct Tunnel {
    pub config: TunnelConfig,
    pub client: RdpClient,
    pub stream: Option<StreamCapture>,
    pub audio: AudioTap,
    pub input_forwarder: Option<InputForwarder>,
    state: TunnelState,
}

impl Tunnel {
    pub fn new(config: TunnelConfig) -> Self {
        let client = RdpClient::new(&config);
        let audio = AudioTap::new(config.audio_redirection);

        Self {
            config,
            client,
            stream: None,
            audio,
            input_forwarder: None,
            state: TunnelState::Idle,
        }
    }

    /// Current lifecycle state, reconciled against client health.
    pub fn state(&mut self) -> TunnelState {
        match self.state {
            TunnelState::Live if !self.client.is_alive() => {
                self.state = TunnelState::Closed;
            }
            TunnelState::Idle if self.client.ever_connected() => {
                self.state = TunnelState::Closed;
            }
            _ => {}
        }
        self.state
    }

    /// Spawn mstsc.exe, wait for the desktop, then attach capture and audio.
    pub async fn connect(&mut self) -> Result<(), RdpError> {
        if self.stream.is_some() {
            return Err(RdpError::AlreadyInitialized("stream"));
        }

        self.state = TunnelState::Connecting;

        if let Err(e) = self.try_connect().await {
            self.state = TunnelState::Closed;
            return Err(e);
        }

        self.state = TunnelState::Live;
        tracing::info!("RDP tunnel is live");
        Ok(())
    }

    async fn try_connect(&mut self) -> Result<(), RdpError> {
        self.client.spawn()?;
        self.client.wait_for_desktop()?;

        // Handle is only valid after spawn + desktop paint.
        let handle = self.client.handle().ok_or(RdpError::NotConnected)?;

        tracing::info!(
            pid = handle.pid(),
            window = %handle.window_title(),
            "RDP client handle acquired; attaching subsystems"
        );

        // Attach capture before marking Live so no frame is missed.
        let capture = StreamCapture::attach(&handle, self.config.capture.clone()).await?;
        self.stream = Some(capture);

        // Audio failures are non-fatal.
        if let Err(e) = self.audio.start(&handle) {
            tracing::warn!(error = %e, "audio tap failed to start; continuing without audio");
        }

        Ok(())
    }

    /// Initialize input forwarding once the session is live.
    pub async fn start_input_forwarding(&mut self) -> Result<(), RdpError> {
        if self.input_forwarder.is_some() {
            return Err(RdpError::AlreadyInitialized("input_forwarder"));
        }

        let handle = self.client.handle().ok_or(RdpError::NotConnected)?;

        if handle.pid() == 0 {
            return Err(RdpError::InvalidPid);
        }

        let forwarder = InputForwarder::new(&handle, self.config.input.clone())?;
        self.input_forwarder = Some(forwarder);
        Ok(())
    }

    pub async fn send_input(&mut self, event: InputEvent) -> Result<(), RdpError> {
        let fwd = self
            .input_forwarder
            .as_mut()
            .ok_or(RdpError::NotConnected)?;
        fwd.send(event)?;
        Ok(())
    }

    pub async fn next_frame(&mut self) -> Result<Frame, RdpError> {
        let stream = self.stream.as_mut().ok_or(RdpError::NotConnected)?;
        Ok(stream.next_frame().await?)
    }

    /// Graceful teardown: input, then stream, then audio, then client.
    pub async fn shutdown(&mut self) -> Result<(), RdpError> {
        self.state = TunnelState::Disconnecting;

        if let Some(mut fwd) = self.input_forwarder.take() {
            if let Err(e) = fwd.shutdown() {
                tracing::warn!(error = ?e, "input forwarder shutdown failed");
            }
        }

        if let Some(stream) = self.stream.take() {
            if let Err(e) = stream.detach().await {
                tracing::warn!(error = ?e, "stream detach failed");
            }
        }

        if let Err(e) = self.audio.stop() {
            tracing::warn!(error = %e, "audio tap stop failed");
        }

        let result = self.client.disconnect().await;
        self.state = TunnelState::Closed;
        result?;
        Ok(())
    }
}