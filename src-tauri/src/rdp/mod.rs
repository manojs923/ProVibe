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
//
// NOTE: the Tunnel/RdpClient/StreamCapture machinery below is not reachable
// from any command yet. The commands are standalone PowerShell wrappers. See
// the wiring note at the bottom.

#![allow(dead_code)]

pub mod audio;
pub mod client;
pub mod input;
pub mod stream;

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::Serialize;
use thiserror::Error;

use crate::hidden_command;
use crate::rdp::audio::AudioTap;
use crate::rdp::client::{ClientError, RdpClient};
use crate::rdp::input::{InputConfig, InputError, InputEvent, InputForwarder};
use crate::rdp::stream::{CaptureConfig, Frame, StreamCapture, StreamError};

/// The one loopback address the whole crate agrees on. `hyperv.rs` binds the
/// Hyper-V vEthernet here and the frontend passes it to `capture_rdp_window`.
pub const LOOPBACK_IP: &str = "127.0.0.2";
/// Adapter name owned by this module.
pub const LOOPBACK_ADAPTER: &str = "ProtocolLoopback";
/// Guest RDP port.
pub const RDP_PORT: u16 = 3389;

// -----------------------------------------------------------------------------
// Contracts expected from sibling modules
// -----------------------------------------------------------------------------
//
// client.rs:
//   pub struct RdpClient; pub struct RDPClientHandle (Clone, Send + Sync);
//   pub enum ClientError (thiserror: Spawn(String), Timeout(Duration),
//       NotRunning, InvalidPath(String), StageCredentials(String), ConfigIo(String));
//   impl RdpClient {
//       pub fn new(cfg: &TunnelConfig) -> Self;
//       pub fn spawn(&mut self) -> Result<(), ClientError>;
//       pub fn wait_for_desktop(&mut self) -> Result<(), ClientError>;
//       pub fn is_alive(&mut self) -> bool;
//       pub fn ever_connected(&self) -> bool;
//       pub fn handle(&self) -> Option<RDPClientHandle>;
//       pub fn kill(&mut self) -> Result<(), ClientError>;
//       pub async fn disconnect(&mut self) -> Result<(), ClientError>;
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
// -----------------------------------------------------------------------------

fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Every adapter/tunnel command goes through `hidden_command` — this binary has
/// no console, so a bare spawn flashes a conhost window per call.
fn run_ps(script: &str) -> Result<String, String> {
    let output = hidden_command("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()
        .map_err(|e| format!("powershell spawn failed: {}", e))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

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

#[derive(serde::Deserialize)]
struct AdapterRow {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Status")]
    status: String,
}

fn adapter_state() -> (String, bool) {
    let script = format!(
        "Get-NetAdapter -Name '{}' -ErrorAction SilentlyContinue | Select-Object Name, Status | ConvertTo-Json -Compress",
        ps_escape(LOOPBACK_ADAPTER)
    );

    let Ok(raw) = run_ps(&script) else {
        return ("None".to_string(), false);
    };
    if raw.is_empty() {
        return ("None".to_string(), false);
    }

    // ConvertTo-Json emits an object for one row, an array for many.
    let rows: Vec<AdapterRow> = match serde_json::from_str::<Vec<AdapterRow>>(&raw) {
        Ok(v) => v,
        Err(_) => match serde_json::from_str::<AdapterRow>(&raw) {
            Ok(v) => vec![v],
            Err(_) => return ("None".to_string(), false),
        },
    };

    match rows.into_iter().next() {
        Some(row) => {
            let up = row.status.eq_ignore_ascii_case("Up");
            (row.name, up)
        }
        None => ("None".to_string(), false),
    }
}

#[tauri::command]
pub fn get_tunnel_status(local_port: Option<u16>) -> Result<TunnelStatus, String> {
    let port = local_port.unwrap_or(RDP_PORT);
    let (adapter_name, adapter_up) = adapter_state();

    let addr = format!("{}:{}", LOOPBACK_IP, port);
    let sock_addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|e: std::net::AddrParseError| e.to_string())?;

    let start = Instant::now();
    let listener_active = TcpStream::connect_timeout(&sock_addr, Duration::from_millis(500)).is_ok();
    let latency_ms = listener_active.then(|| start.elapsed().as_millis() as u64);

    Ok(TunnelStatus {
        active: listener_active && adapter_up,
        local_port: port,
        remote_host: LOOPBACK_IP.to_string(),
        remote_port: port,
        latency_ms,
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
    let adapter = adapter_name.unwrap_or_else(|| LOOPBACK_ADAPTER.to_string());
    let ip = ip_address.unwrap_or_else(|| LOOPBACK_IP.to_string());
    let adapter_esc = ps_escape(&adapter);
    let ip_esc = ps_escape(&ip);

    // The Microsoft KM-TEST loopback adapter is not a Windows feature; it is an
    // INF install. `netcfg` is the reliable route, `devcon` is the WDK fallback.
    let install_script = format!(
        r#"$ErrorActionPreference = 'Stop'
$name = '{adapter}'
$existing = Get-NetAdapter -Name $name -ErrorAction SilentlyContinue
if (-not $existing) {{
    $lo = Get-NetAdapter -ErrorAction SilentlyContinue |
          Where-Object {{ $_.InterfaceDescription -like '*Loopback*' }} |
          Select-Object -First 1
    if (-not $lo) {{
        netcfg -l $env:WINDIR\inf\netloop.inf -c s -i ms_loopback | Out-Null
        Start-Sleep -Seconds 3
        $lo = Get-NetAdapter -ErrorAction SilentlyContinue |
              Where-Object {{ $_.InterfaceDescription -like '*Loopback*' }} |
              Select-Object -First 1
    }}
    if (-not $lo) {{
        $devcon = "C:\Program Files (x86)\Windows Kits\10\Tools\x64\devcon.exe"
        if (Test-Path $devcon) {{
            & $devcon install $env:WINDIR\INF\netloop.inf *MSLOOP | Out-Null
            Start-Sleep -Seconds 3
            $lo = Get-NetAdapter -ErrorAction SilentlyContinue |
                  Where-Object {{ $_.InterfaceDescription -like '*Loopback*' }} |
                  Select-Object -First 1
        }}
    }}
    if (-not $lo) {{ throw 'loopback adapter could not be installed (need admin + netcfg)' }}
    Rename-NetAdapter -Name $lo.Name -NewName $name -ErrorAction SilentlyContinue
}}
if (-not (Get-NetAdapter -Name $name -ErrorAction SilentlyContinue)) {{
    throw 'loopback adapter missing after install'
}}
Enable-NetAdapter -Name $name -Confirm:$false -ErrorAction SilentlyContinue
"#,
        adapter = adapter_esc
    );

    run_ps(&install_script)?;

    // Clear any stacked address from a previous run before assigning.
    let ip_script = format!(
        r#"$ErrorActionPreference = 'Stop'
$name = '{adapter}'
Get-NetIPAddress -InterfaceAlias $name -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Remove-NetIPAddress -Confirm:$false -ErrorAction SilentlyContinue
New-NetIPAddress -InterfaceAlias $name -IPAddress {ip} -PrefixLength 24 -ErrorAction Stop | Out-Null
Set-NetIPInterface -InterfaceAlias $name -InterfaceMetric 5
"#,
        adapter = adapter_esc,
        ip = ip_esc
    );

    let ip_ok = run_ps(&ip_script).is_ok();

    // Host-side RDP listener; best-effort, needs elevation.
    let _ = run_ps(
        "Set-ItemProperty -Path 'HKLM:\\System\\CurrentControlSet\\Control\\Terminal Server' \
         -Name 'fDenyTSConnections' -Value 0; \
         Enable-NetFirewallRule -DisplayGroup 'Remote Desktop'",
    );

    Ok(TunnelStartResult {
        success: ip_ok,
        adapter_name: adapter,
        ip_address: ip,
        message: if ip_ok {
            "loopback tunnel established".to_string()
        } else {
            "adapter present but IP assignment failed".to_string()
        },
    })
}

#[tauri::command]
pub fn stop_loopback_tunnel(adapter_name: Option<String>) -> Result<String, String> {
    let adapter = adapter_name.unwrap_or_else(|| LOOPBACK_ADAPTER.to_string());

    let script = format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
$name = '{adapter}'
if (-not (Get-NetAdapter -Name $name -ErrorAction SilentlyContinue)) {{ Write-Output 'NotFound'; exit }}
Get-NetIPAddress -InterfaceAlias $name -AddressFamily IPv4 |
    Remove-NetIPAddress -Confirm:$false -ErrorAction SilentlyContinue
Disable-NetAdapter -Name $name -Confirm:$false -ErrorAction SilentlyContinue
Write-Output 'Disabled'
"#,
        adapter = ps_escape(&adapter)
    );

    let output = run_ps(&script)?;

    match output.trim() {
        "Disabled" => Ok(format!("tunnel adapter '{}' disabled and unbind", adapter)),
        "NotFound" => Ok("no active tunnel found".to_string()),
        other => Err(format!("unexpected result: {}", other)),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    Idle,
    Connecting,
    Live,
    Disconnecting,
    Closed,
}

#[derive(Debug, Clone)]
pub struct TunnelConfig {
    pub loopback_addr: String,
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
            loopback_addr: LOOPBACK_IP.to_string(),
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
    ///
    /// `wait_for_desktop()` blocks up to its timeout, so call this from a
    /// blocking task rather than an executor-critical path.
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

        let handle = self.client.handle().ok_or(RdpError::NotConnected)?;

        tracing::info!(
            pid = handle.pid(),
            window = %handle.window_title(),
            "RDP client handle acquired; attaching subsystems"
        );

        let capture = StreamCapture::attach(&handle, self.config.capture.clone()).await?;
        self.stream = Some(capture);

        if let Err(e) = self.audio.start(&handle) {
            tracing::warn!(error = %e, "audio tap failed to start; continuing without audio");
        }

        Ok(())
    }

    pub async fn start_input_forwarding(&mut self) -> Result<(), RdpError> {
        if self.input_forwarder.is_some() {
            return Err(RdpError::AlreadyInitialized("input_forwarder"));
        }

        let handle = self.client.handle().ok_or(RdpError::NotConnected)?;
        if handle.pid() == 0 {
            return Err(RdpError::InvalidPid);
        }

        self.input_forwarder = Some(InputForwarder::new(&handle, self.config.input.clone())?);
        Ok(())
    }

    pub fn send_input(&mut self, event: InputEvent) -> Result<(), RdpError> {
        self.input_forwarder
            .as_mut()
            .ok_or(RdpError::NotConnected)?
            .send(event)?;
        Ok(())
    }

    pub async fn next_frame(&mut self) -> Result<Frame, RdpError> {
        let stream = self.stream.as_mut().ok_or(RdpError::NotConnected)?;
        Ok(stream.next_frame().await?)
    }

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

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(mut fwd) = self.input_forwarder.take() {
            let _ = fwd.shutdown();
        }
        self.stream.take();
        let _ = self.audio.stop();
        let _ = self.client.kill();
    }
}