// src-tauri/src/stealth/mod.rs

pub mod artifact;
pub mod cpuid;
pub mod hardware;
pub mod timing;

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sysinfo::System;
use tauri::AppHandle;
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

// Re-exported for downstream consumers; the kind/override pair is not read
// inside this module, hence the allow.
#[allow(unused_imports)]
pub use crate::target_id::{
    DetectionVector, DetectionVectorKind, TargetId, TargetProfile, VMConfigOverride,
};

use crate::patches::loader;
use crate::stealth::artifact::{
    run_full_cleanup, run_full_restore, CleanupReport, GuestArtifactError,
};
use crate::stealth::cpuid::{
    build_mask_script, build_unmask_script, run_powershell_on_host, run_powershell_on_host_sync,
};
use crate::stealth::hardware::{
    patch_disk_serials, patch_mac_addresses, restore_disk_serials, restore_mac_addresses,
    restore_smbios_profile, HardwareProfile,
};
use crate::vm::config::build_config;
use crate::vm::lifecycle::VmHandle;

// ---------------------------------------------------------------------------
// Public entrypoint / bootstrapping
// ---------------------------------------------------------------------------

/// Convenience bootstrap: apply a baseline patch set using the given AppHandle.
/// The actual target is resolved via `build_config` + the loader.
pub async fn bootstrap_default(app_handle: &AppHandle) {
    let target_id = "MSB"; // Default target ID — override via IPC command for real runs.
    let vm_config = build_config(target_id);

    if let Err(e) = loader::apply_patches(vec![], &vm_config).await {
        eprintln!(
            "[stealth::bootstrap_default] Failed to apply patches: {}",
            e
        );
    } else {
        println!(
            "[stealth::bootstrap_default] Patches applied successfully for '{}'.",
            target_id
        );
    }

    // Reserved for future IPC event emission.
    let _ = app_handle;
}

/// Block-on helper for sync contexts that need to call async code.
/// MUST be invoked from within a Tokio runtime.
pub(crate) fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Handle::current().block_on(fut)
}

// ---------------------------------------------------------------------------
// StealthCheck / StealthPosture — IPC-exposed health report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct StealthCheck {
    pub check_name: String,
    pub passed: bool,
    pub status: String,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StealthPosture {
    pub overall_score: u8,
    pub total_checks: usize,
    pub passed_checks: usize,
    pub failed_checks: usize,
    pub checks: Vec<StealthCheck>,
    pub posture_level: String,
}

/// Substring match against a process name, using an already-refreshed table.
///
/// Takes the System by reference: the posture sweep runs eleven of these, and
/// rebuilding the table per call would re-enumerate every process each time.
fn process_running(sys: &System, signature: &str) -> bool {
    let needle = signature.to_lowercase();
    sys.processes()
        .values()
        .any(|p| p.name().to_lowercase().contains(&needle))
}

fn powershell_scalar(script: &str) -> String {
    Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[tauri::command]
pub fn verify_stealth_posture() -> Result<StealthPosture, String> {
    let mut checks: Vec<StealthCheck> = Vec::new();

    // One process snapshot for every process-based check below.
    let mut sys = System::new_all();
    sys.refresh_processes();

    // Check 1: Hyper-V management service
    let hyperv_running = process_running(&sys, "vmms");
    checks.push(StealthCheck {
        check_name: "hyperv_management".to_string(),
        passed: true,
        status: if hyperv_running {
            "Hyper-V VM Management running (normal)".to_string()
        } else {
            "Hyper-V VM Management not running — VMs won't start".to_string()
        },
        remediation: if !hyperv_running {
            Some("Start-Service vmms".to_string())
        } else {
            None
        },
    });

    // Check 2: RDP / VMConnect window available for optical capture
    let rdp_procs = ["mstsc", "vmconnect", "rdpclient"];
    let rdp_found = rdp_procs.iter().any(|p| process_running(&sys, p));
    checks.push(StealthCheck {
        check_name: "rdp_window_present".to_string(),
        passed: rdp_found,
        status: if rdp_found {
            "RDP/VM window process detected — capture available".to_string()
        } else {
            "No RDP or VMConnect window open — nothing to capture".to_string()
        },
        remediation: if !rdp_found {
            Some("Start the RDP session or launch VMConnect before activating overlay".to_string())
        } else {
            None
        },
    });

    // Check 3: Proctoring software detection (informational)
    let proctor_sigs = [
        "proctorio",
        "honorlock",
        "proctoru",
        "respondus",
        "lockdownbrowser",
        "examity",
        "testinvite",
    ];
    let running_proctors: Vec<String> = proctor_sigs
        .iter()
        .filter(|sig| process_running(&sys, sig))
        .map(|s| s.to_string())
        .collect();

    checks.push(StealthCheck {
        check_name: "proctoring_software".to_string(),
        passed: true,
        status: if running_proctors.is_empty() {
            "No proctoring software detected on Host".to_string()
        } else {
            format!(
                "Active proctoring: {} (Guest-side only — optical gap holds)",
                running_proctors.join(", ")
            )
        },
        remediation: None,
    });

    // Check 4: Overlay WDA_EXCLUDEFROMCAPTURE
    let overlay_script = r#"
        Add-Type -TypeDefinition @'
        using System;
        using System.Runtime.InteropServices;
        public class Win32 {
            [DllImport("user32.dll")]
            public static extern int GetWindowLong(IntPtr hWnd, int nIndex);
            [DllImport("user32.dll")]
            public static extern int GetWindowDisplayAffinity(IntPtr hWnd, out uint affinity);
        }
'@
        $proc = Get-Process | Where-Object { $_.MainWindowTitle -ne '' -and $_.ProcessName -match 'protocol|tauri' } | Select-Object -First 1
        if ($proc) {
            $hwnd = $proc.MainWindowHandle
            $affinity = 0
            [Win32]::GetWindowDisplayAffinity($hwnd, [ref]$affinity) | Out-Null
            Write-Output $affinity
        } else {
            Write-Output 'no-window'
        }
    "#;

    let affinity_result = powershell_scalar(overlay_script);
    let overlay_protected = affinity_result == "17"; // WDA_EXCLUDEFROMCAPTURE = 0x11
    checks.push(StealthCheck {
        check_name: "overlay_capture_protection".to_string(),
        passed: overlay_protected,
        status: match affinity_result.as_str() {
            "17" => "WDA_EXCLUDEFROMCAPTURE active — overlay invisible to screen share".to_string(),
            "no-window" => "Overlay window not found — is the app running?".to_string(),
            "0" => "Overlay visible to capture APIs — NOT protected".to_string(),
            other => format!("Display affinity returned: {}", other),
        },
        remediation: if !overlay_protected {
            Some("Ensure Windows 10 2004+ and overlay is calling SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)".to_string())
        } else {
            None
        },
    });

    // Check 5: Loopback adapter
    let adapter_result = powershell_scalar(
        "(Get-NetAdapter | Where-Object { $_.Name -match 'Loopback|Protocol' } | Measure-Object).Count",
    );
    let tunnel_ready = adapter_result.parse::<u32>().unwrap_or(0) > 0;
    checks.push(StealthCheck {
        check_name: "loopback_tunnel".to_string(),
        passed: true,
        status: if tunnel_ready {
            "Loopback adapter present — tunnel mode available".to_string()
        } else {
            "No loopback adapter — tunnel mode unavailable (local capture still works)".to_string()
        },
        remediation: if !tunnel_ready {
            Some("Run start_loopback_tunnel to provision the adapter".to_string())
        } else {
            None
        },
    });

    // Check 6: RDP listener on 3389
    let port_check = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:3389"
            .parse::<std::net::SocketAddr>()
            .map_err(|e| e.to_string())?,
        std::time::Duration::from_millis(300),
    )
    .is_ok();

    checks.push(StealthCheck {
        check_name: "rdp_listener".to_string(),
        passed: port_check,
        status: if port_check {
            "RDP listener active on 3389".to_string()
        } else {
            "RDP not listening on 3389 — Guest connection may fail".to_string()
        },
        remediation: if !port_check {
            Some("Enable RDP in Guest OS or use vmconnect instead".to_string())
        } else {
            None
        },
    });

    // Check 7: Defender real-time
    let defender_result = powershell_scalar(
        "(Get-MpPreference | Select-Object -ExpandProperty DisableRealtimeMonitoring)",
    );
    let defender_off = defender_result.contains("True");
    checks.push(StealthCheck {
        check_name: "defender_realtime".to_string(),
        passed: true,
        status: if defender_off {
            "Defender real-time monitoring disabled".to_string()
        } else {
            "Defender real-time monitoring ACTIVE — may scan overlay binary".to_string()
        },
        remediation: if !defender_off {
            Some("Add exclusion folder for the protocol binary if scans cause lag".to_string())
        } else {
            None
        },
    });

    let passed = checks.iter().filter(|c| c.passed).count();
    let total = checks.len();
    let failed = total - passed;

    let score = ((passed as f32 / total as f32) * 100.0) as u8;
    let posture_level = match score {
        90..=100 => "GREEN".to_string(),
        70..=89 => "YELLOW".to_string(),
        0..=69 => "RED".to_string(),
        _ => "UNKNOWN".to_string(),
    };

    Ok(StealthPosture {
        overall_score: score,
        total_checks: total,
        passed_checks: passed,
        failed_checks: failed,
        checks,
        posture_level,
    })
}

// ---------------------------------------------------------------------------
// StealthError — canonical error type for the stealth subsystem
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StealthError {
    #[error("VM handle invalid or VM not running: {0}")]
    InvalidHandle(String),

    #[error("PowerShell execution failed (exit {exit_code}): {stderr}")]
    PowerShellFailure { exit_code: i32, stderr: String },

    #[error("Target profile missing required stealth configuration: {0}")]
    MissingConfig(String),

    #[error("CPUID mask application failed: {0}")]
    CpuidFailure(String),

    #[error("Hardware spoofing failed for device class '{device_class}': {reason}")]
    HardwareFailure { device_class: String, reason: String },

    #[error("Artifact cleanup failed for registry key '{key}': {reason}")]
    ArtifactFailure { key: String, reason: String },

    #[error("Timing normalization failed: {0}")]
    TimingFailure(String),

    #[error("Partial application: {applied}/{total} stealth layers succeeded")]
    PartialApplication { applied: usize, total: usize },

    #[error("VM went offline during stealth application")]
    VmLost,

    #[error("Script execution failed: {0}")]
    ScriptFailure(String),

    #[error("Verification failed: {0}")]
    VerificationFailed(String),

    #[error("Host invocation error: {0}")]
    HostInvocation(String),

    /// Guest artifact sweep failure. Carried as the concrete error rather
    /// than a string so callers can inspect the variant; the conversion
    /// itself lives in artifact.rs.
    #[error("Guest artifact error: {0}")]
    GuestArtifact(GuestArtifactError),

    /// Guest Services / PowerShell Direct transport fault.
    #[error("Guest services error: {0}")]
    GuestServices(String),

    #[error("Session not found: {id}")]
    SessionNotFound { id: Uuid },

    #[error("Invalid transition from {from} to {to}")]
    InvalidTransition { from: String, to: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type StealthResult<T> = Result<T, StealthError>;

// ---------------------------------------------------------------------------
// StealthLayer — layer identifiers + ordering
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StealthLayer {
    CpuIdMask,
    HardwareSpoof,
    ArtifactCleanup,
    TimingNormalize,
}

impl StealthLayer {
    pub const ALL: [StealthLayer; 4] = [
        StealthLayer::CpuIdMask,
        StealthLayer::HardwareSpoof,
        StealthLayer::ArtifactCleanup,
        StealthLayer::TimingNormalize,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            StealthLayer::CpuIdMask => "cpuid_mask",
            StealthLayer::HardwareSpoof => "hardware_spoof",
            StealthLayer::ArtifactCleanup => "artifact_cleanup",
            StealthLayer::TimingNormalize => "timing_normalize",
        }
    }

    pub fn execution_order(&self) -> u8 {
        match self {
            StealthLayer::CpuIdMask => 0,
            StealthLayer::HardwareSpoof => 1,
            StealthLayer::ArtifactCleanup => 2,
            StealthLayer::TimingNormalize => 3,
        }
    }
}

// ---------------------------------------------------------------------------
// StealthRequirements — derived from TargetProfile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct StealthRequirements {
    pub layers: Option<Vec<StealthLayer>>,
    pub skip_timing: bool,
    pub force_hardware_spoof: bool,
}

impl Default for StealthRequirements {
    fn default() -> Self {
        Self {
            layers: None,
            skip_timing: false,
            force_hardware_spoof: false,
        }
    }
}

impl StealthRequirements {
    pub fn from_profile(profile: &TargetProfile) -> Self {
        let dv = &profile.detection_vectors;
        let dv_kinds = dv.kinds();

        let needs_timing_spoof = profile
            .vm_config_override
            .as_ref()
            .map(|c| c.strict_timing_spoof)
            .unwrap_or(false);

        let skip_timing = !needs_timing_spoof;

        let mut layers: Vec<StealthLayer> = StealthLayer::ALL.to_vec();

        if skip_timing {
            layers.retain(|l| *l != StealthLayer::TimingNormalize);
        }

        let force_hardware_spoof = profile.patch_set_id == "generic"
            || profile.target_id == TargetId::Generic
            || dv_kinds.is_empty();

        Self {
            layers: Some(layers),
            skip_timing,
            force_hardware_spoof,
        }
    }

    fn ordered_layers(&self) -> Vec<StealthLayer> {
        let mut layers: Vec<StealthLayer> = self
            .layers
            .clone()
            .unwrap_or_else(|| StealthLayer::ALL.to_vec());
        layers.sort_by_key(|l| l.execution_order());
        layers.dedup();
        layers
    }
}

// ---------------------------------------------------------------------------
// Session support types — state machine, counters, detections
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Initializing,
    Active,
    Degraded,
    Suspended,
    Compromised,
    Terminated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Countermeasure {
    CpuidSpoof,
    HardwareSpoof,
    ArtifactCleanup,
    TimingNormalize,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionEvent {
    pub timestamp: u64,
    pub vector: DetectionVector,
    pub severity: Severity,
    pub source: String,
    pub details: String,
}

impl DetectionEvent {
    pub fn new(
        vector: DetectionVector,
        severity: Severity,
        source: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            vector,
            severity,
            source: source.into(),
            details: details.into(),
        }
    }
}

/// Per-session stealth profile — thresholds and defaults used by the session
/// state machine. `Default` keeps a sane failure threshold so a session is not
/// instantly marked compromised.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StealthProfile {
    pub failure_threshold: u32,
    pub patch_set_id: String,
}

impl Default for StealthProfile {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            patch_set_id: "generic".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// StealthReport / StealthSession — per-run state tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct StealthReport {
    pub target_id: String,
    pub applied_layers: Vec<StealthLayer>,
    pub failed_layers: Vec<(StealthLayer, String)>,
    pub duration_ms: u128,
}

impl StealthReport {
    pub fn fully_applied(&self) -> bool {
        self.failed_layers.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct StealthSession {
    pub report: StealthReport,
    pub session_id: Uuid,
    pub profile: StealthProfile,
    pub target_id: TargetId,
    pub created_at: u64,
    pub last_activity: u64,
    pub active_countermeasures: Vec<Countermeasure>,
    pub detection_events: Vec<DetectionEvent>,
    pub success_count: u32,
    pub failure_count: u32,
    pub state: SessionState,
}

pub type SharedSession = Arc<RwLock<StealthSession>>;

impl StealthSession {
    pub fn new(profile: StealthProfile, target_id: TargetId) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            report: StealthReport::default(),
            session_id: Uuid::new_v4(),
            profile,
            target_id,
            created_at: now,
            last_activity: now,
            active_countermeasures: Vec::new(),
            detection_events: Vec::new(),
            success_count: 0,
            failure_count: 0,
            state: SessionState::Initializing,
        }
    }

    pub fn record_success(&mut self, countermeasure: Countermeasure) {
        self.success_count = self.success_count.saturating_add(1);
        self.touch();
        if !self.active_countermeasures.contains(&countermeasure) {
            self.active_countermeasures.push(countermeasure);
        }
    }

    pub fn record_failure(&mut self, event: DetectionEvent) {
        self.failure_count = self.failure_count.saturating_add(1);
        self.detection_events.push(event);
        self.touch();

        if self.failure_count >= self.profile.failure_threshold {
            self.state = SessionState::Compromised;
        }
    }

    pub fn touch(&mut self) {
        self.last_activity = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
    }

    pub fn transition(&mut self, new_state: SessionState) -> Result<(), StealthError> {
        use SessionState::*;

        let valid = matches!(
            (&self.state, &new_state),
            (Initializing, Active)
                | (Active, Degraded)
                | (Active, Suspended)
                | (Degraded, Active)
                | (Degraded, Compromised)
                | (Suspended, Active)
                | (Suspended, Terminated)
                | (_, Terminated)
                | (_, Compromised)
        );

        if !valid {
            return Err(StealthError::InvalidTransition {
                from: format!("{:?}", self.state),
                to: format!("{:?}", new_state),
            });
        }

        self.state = new_state;
        self.touch();
        Ok(())
    }

    pub fn is_viable(&self) -> bool {
        matches!(self.state, SessionState::Active | SessionState::Degraded)
    }

    pub fn success_rate(&self) -> f64 {
        let total = self.success_count + self.failure_count;
        if total == 0 {
            return 1.0;
        }
        self.success_count as f64 / total as f64
    }
}

// ---------------------------------------------------------------------------
// Session Manager
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct StealthManager {
    sessions: Arc<RwLock<HashMap<Uuid, StealthSession>>>,
    default_profile: StealthProfile,
    metrics: Arc<RwLock<StealthMetrics>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StealthMetrics {
    pub total_sessions: u64,
    pub active_sessions: u64,
    pub compromised_sessions: u64,
    pub total_detections: u64,
    pub total_successes: u64,
}

impl StealthManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            default_profile: StealthProfile::default(),
            metrics: Arc::new(RwLock::new(StealthMetrics::default())),
        }
    }

    pub fn with_profile(default_profile: StealthProfile) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            default_profile,
            metrics: Arc::new(RwLock::new(StealthMetrics::default())),
        }
    }

    /// Create a session from the manager's default profile.
    pub async fn create_session(&self, target_id: TargetId) -> StealthResult<Uuid> {
        self.create_session_with_profile(target_id, self.default_profile.clone())
            .await
    }

    /// Create a session with an explicit profile. The session starts in
    /// `Initializing` and must be activated explicitly.
    pub async fn create_session_with_profile(
        &self,
        target_id: TargetId,
        profile: StealthProfile,
    ) -> StealthResult<Uuid> {
        if profile.failure_threshold == 0 {
            return Err(StealthError::MissingConfig(
                "failure_threshold must be greater than zero".to_string(),
            ));
        }

        let session = StealthSession::new(profile, target_id);
        let id = session.session_id;

        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(id, session);
        }

        {
            let mut m = self.metrics.write().await;
            m.total_sessions = m.total_sessions.saturating_add(1);
        }

        Ok(id)
    }

    pub async fn get_session(&self, id: Uuid) -> StealthResult<StealthSession> {
        let sessions = self.sessions.read().await;
        sessions
            .get(&id)
            .cloned()
            .ok_or(StealthError::SessionNotFound { id })
    }

    /// Mutate a session in place under the write lock.
    pub async fn with_session_mut<F, T>(&self, id: Uuid, f: F) -> StealthResult<T>
    where
        F: FnOnce(&mut StealthSession) -> T,
    {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(&id)
            .ok_or(StealthError::SessionNotFound { id })?;
        Ok(f(session))
    }

    pub async fn activate_session(&self, id: Uuid) -> StealthResult<()> {
        let previous = {
            let mut sessions = self.sessions.write().await;
            let session = sessions
                .get_mut(&id)
                .ok_or(StealthError::SessionNotFound { id })?;
            let previous = session.state;
            session.transition(SessionState::Active)?;
            previous
        };

        if previous == SessionState::Initializing {
            let mut m = self.metrics.write().await;
            m.active_sessions = m.active_sessions.saturating_add(1);
        }

        Ok(())
    }

    pub async fn terminate_session(&self, id: Uuid) -> StealthResult<()> {
        let (was_active, was_compromised) = {
            let mut sessions = self.sessions.write().await;
            let session = sessions
                .get_mut(&id)
                .ok_or(StealthError::SessionNotFound { id })?;
            let was_active = matches!(session.state, SessionState::Active | SessionState::Degraded);
            let was_compromised = session.state == SessionState::Compromised;
            session.transition(SessionState::Terminated)?;
            (was_active, was_compromised)
        };

        let mut m = self.metrics.write().await;
        if was_active {
            m.active_sessions = m.active_sessions.saturating_sub(1);
        }
        if was_compromised {
            m.compromised_sessions = m.compromised_sessions.saturating_sub(1);
        }

        Ok(())
    }

    pub async fn remove_session(&self, id: Uuid) -> StealthResult<()> {
        let mut sessions = self.sessions.write().await;
        sessions
            .remove(&id)
            .map(|_| ())
            .ok_or(StealthError::SessionNotFound { id })
    }

    pub async fn list_sessions(&self) -> Vec<Uuid> {
        let sessions = self.sessions.read().await;
        sessions.keys().copied().collect()
    }

    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    // -- detection + countermeasure accounting ------------------------------

    pub async fn record_detection(
        &self,
        id: Uuid,
        event: DetectionEvent,
    ) -> StealthResult<SessionState> {
        let (state, newly_compromised) = {
            let mut sessions = self.sessions.write().await;
            let session = sessions
                .get_mut(&id)
                .ok_or(StealthError::SessionNotFound { id })?;

            let was_compromised = session.state == SessionState::Compromised;
            session.record_failure(event);
            let now_compromised = session.state == SessionState::Compromised;

            (session.state, !was_compromised && now_compromised)
        };

        {
            let mut m = self.metrics.write().await;
            m.total_detections = m.total_detections.saturating_add(1);
            // Only count the transition, so repeat detections don't inflate
            // the compromised gauge.
            if newly_compromised {
                m.compromised_sessions = m.compromised_sessions.saturating_add(1);
            }
        }

        Ok(state)
    }

    pub async fn record_success(
        &self,
        id: Uuid,
        countermeasure: Countermeasure,
    ) -> StealthResult<()> {
        {
            let mut sessions = self.sessions.write().await;
            let session = sessions
                .get_mut(&id)
                .ok_or(StealthError::SessionNotFound { id })?;
            session.record_success(countermeasure);
        }

        let mut m = self.metrics.write().await;
        m.total_successes = m.total_successes.saturating_add(1);
        Ok(())
    }

    // -- metrics ------------------------------------------------------------

    pub async fn metrics_snapshot(&self) -> StealthMetrics {
        self.metrics.read().await.clone()
    }

    pub async fn reset_metrics(&self) {
        let mut m = self.metrics.write().await;
        *m = StealthMetrics::default();
    }

    /// Recompute derived gauges (active/compromised) from live sessions rather
    /// than trusting the incrementally maintained counters.
    pub async fn reconcile_metrics(&self) -> StealthMetrics {
        let sessions = self.sessions.read().await;

        let active = sessions
            .values()
            .filter(|s| matches!(s.state, SessionState::Active | SessionState::Degraded))
            .count() as u64;
        let compromised = sessions
            .values()
            .filter(|s| s.state == SessionState::Compromised)
            .count() as u64;
        let detections: u64 = sessions
            .values()
            .map(|s| s.detection_events.len() as u64)
            .sum();
        let successes: u64 = sessions.values().map(|s| s.success_count as u64).sum();
        let total = sessions.len() as u64;

        drop(sessions);

        let mut m = self.metrics.write().await;
        m.total_sessions = m.total_sessions.max(total);
        m.active_sessions = active;
        m.compromised_sessions = compromised;
        m.total_detections = detections;
        m.total_successes = successes;
        m.clone()
    }
}

impl Default for StealthManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Posture checks against a live session
// ---------------------------------------------------------------------------

impl StealthManager {
    pub async fn posture_for(&self, id: Uuid) -> StealthResult<StealthPosture> {
        let session = self.get_session(id).await?;

        let mut checks = Vec::new();

        checks.push(StealthCheck {
            check_name: "session_state".to_string(),
            passed: session.is_viable(),
            status: format!("Session state: {:?}", session.state),
            remediation: (!session.is_viable())
                .then(|| "Activate or restore the session before relying on it".to_string()),
        });

        checks.push(StealthCheck {
            check_name: "failure_budget".to_string(),
            passed: session.failure_count < session.profile.failure_threshold,
            status: format!(
                "{}/{} failures before compromise",
                session.failure_count, session.profile.failure_threshold
            ),
            remediation: (session.failure_count >= session.profile.failure_threshold)
                .then(|| "Reapply stealth layers or start a fresh session".to_string()),
        });

        checks.push(StealthCheck {
            check_name: "countermeasure_coverage".to_string(),
            passed: !session.active_countermeasures.is_empty(),
            status: format!(
                "{} countermeasure(s) reported",
                session.active_countermeasures.len()
            ),
            remediation: session.active_countermeasures.is_empty().then(|| {
                "Report at least one successful countermeasure application".to_string()
            }),
        });

        let total = checks.len();
        let passed = checks.iter().filter(|c| c.passed).count();
        let failed = total - passed;
        let score = ((passed as f32 / total as f32) * 100.0) as u8;

        let posture_level = match score {
            90..=100 => "GREEN",
            70..=89 => "YELLOW",
            _ => "RED",
        }
        .to_string();

        Ok(StealthPosture {
            overall_score: score,
            total_checks: total,
            passed_checks: passed,
            failed_checks: failed,
            checks,
            posture_level,
        })
    }
}

// ---------------------------------------------------------------------------
// Layer runner
// ---------------------------------------------------------------------------

/// Apply requested stealth layers in canonical order, recording each outcome.
/// Partial success is reported rather than collapsed into a single error.
///
/// `vm_name` is taken from the handle because the CPUID and hardware layers
/// address the guest by name, while the artifact layer needs the handle itself.
pub async fn apply_layers(
    requirements: &StealthRequirements,
    vm: &VmHandle,
    profile: &HardwareProfile,
) -> StealthResult<StealthReport> {
    let started = Instant::now();
    let mut report = StealthReport::default();

    let vm_name = vm.vm_name();
    let layers = requirements.ordered_layers();
    let total = layers.len();

    for layer in layers {
        let outcome: StealthResult<()> = match layer {
            StealthLayer::CpuIdMask => {
                let script = build_mask_script(vm_name);
                run_powershell_on_host(&script).await.map(|_| ())
            }
            StealthLayer::HardwareSpoof => patch_mac_addresses(vm_name, profile)
                .await
                .map_err(|e| StealthError::HardwareFailure {
                    device_class: "network".to_string(),
                    reason: e.to_string(),
                }),
            StealthLayer::ArtifactCleanup => {
                run_full_cleanup(vm)
                    .await
                    .map(|_: CleanupReport| ())
                    .map_err(|e| StealthError::ArtifactFailure {
                        key: "registry".to_string(),
                        reason: e.to_string(),
                    })
            }
            StealthLayer::TimingNormalize => patch_disk_serials(vm_name, profile)
                .await
                .map_err(|e| StealthError::TimingFailure(e.to_string())),
        };

        match outcome {
            Ok(()) => report.applied_layers.push(layer),
            Err(e) => report.failed_layers.push((layer, e.to_string())),
        }
    }

    report.duration_ms = started.elapsed().as_millis();

    if report.applied_layers.is_empty() && total > 0 {
        return Err(StealthError::PartialApplication {
            applied: 0,
            total,
        });
    }

    Ok(report)
}

/// Reverse every applied layer, best-effort. Errors are collected, not fatal.
pub async fn revert_layers(
    report: &StealthReport,
    vm: &VmHandle,
    profile: &HardwareProfile,
) -> Vec<(StealthLayer, String)> {
    let mut errors = Vec::new();
    let vm_name = vm.vm_name();

    for layer in report.applied_layers.iter().rev() {
        let result: Result<(), String> = match layer {
            StealthLayer::CpuIdMask => {
                let script = build_unmask_script(vm_name);
                run_powershell_on_host_sync(&script)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
            StealthLayer::HardwareSpoof => {
                async {
                    restore_mac_addresses(vm_name)
                        .await
                        .map_err(|e| e.to_string())?;
                    restore_disk_serials(vm_name)
                        .await
                        .map_err(|e| e.to_string())?;
                    restore_smbios_profile(vm_name)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok::<(), String>(())
                }
                .await
            }
            StealthLayer::ArtifactCleanup => run_full_restore(vm)
                .await
                .map(|_: CleanupReport| ())
                .map_err(|e| e.to_string()),
            StealthLayer::TimingNormalize => Ok(()),
        };

        if let Err(e) = result {
            errors.push((*layer, e));
        }
    }

    errors
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target_id::TargetId;

    fn manager() -> StealthManager {
        StealthManager::new()
    }

    fn event() -> DetectionEvent {
        DetectionEvent::new(
            DetectionVector::default(),
            Severity::High,
            "test",
            "synthetic detection",
        )
    }

    #[tokio::test]
    async fn session_creation_records_id() {
        let m = manager();
        let id = m.create_session(TargetId::Generic).await.unwrap();

        assert_eq!(m.session_count().await, 1);
        assert!(m.list_sessions().await.contains(&id));

        let session = m.get_session(id).await.unwrap();
        assert_eq!(session.state, SessionState::Initializing);
        assert!(session.active_countermeasures.is_empty());
        assert_eq!(session.success_rate(), 1.0);
    }

    #[tokio::test]
    async fn zero_threshold_is_rejected() {
        let m = manager();
        let profile = StealthProfile {
            failure_threshold: 0,
            patch_set_id: "generic".into(),
        };
        let err = m
            .create_session_with_profile(TargetId::Generic, profile)
            .await
            .unwrap_err();
        assert!(matches!(err, StealthError::MissingConfig(_)));
    }

    #[tokio::test]
    async fn unknown_session_is_not_found() {
        let m = manager();
        let fake = Uuid::new_v4();
        let err = m.get_session(fake).await.unwrap_err();
        match err {
            StealthError::SessionNotFound { id } => assert_eq!(id, fake),
            other => panic!("expected SessionNotFound, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn valid_transitions_succeed() {
        let m = manager();
        let id = m.create_session(TargetId::Generic).await.unwrap();

        m.activate_session(id).await.unwrap();
        assert_eq!(m.get_session(id).await.unwrap().state, SessionState::Active);

        m.with_session_mut(id, |s| s.transition(SessionState::Degraded))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            m.get_session(id).await.unwrap().state,
            SessionState::Degraded
        );

        m.with_session_mut(id, |s| s.transition(SessionState::Active))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.get_session(id).await.unwrap().state, SessionState::Active);
    }

    #[tokio::test]
    async fn invalid_transition_is_rejected() {
        let m = manager();
        let id = m.create_session(TargetId::Generic).await.unwrap();

        // Initializing -> Degraded is not in the table.
        let err = m
            .with_session_mut(id, |s| s.transition(SessionState::Degraded))
            .await
            .unwrap()
            .unwrap_err();

        match err {
            StealthError::InvalidTransition { from, to } => {
                assert_eq!(from, "Initializing");
                assert_eq!(to, "Degraded");
            }
            other => panic!("expected InvalidTransition, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn terminate_moves_to_terminated_and_keeps_session() {
        let m = manager();
        let id = m.create_session(TargetId::Generic).await.unwrap();
        m.activate_session(id).await.unwrap();
        m.terminate_session(id).await.unwrap();

        assert_eq!(
            m.get_session(id).await.unwrap().state,
            SessionState::Terminated
        );
        assert_eq!(m.session_count().await, 1);
    }

    #[tokio::test]
    async fn detection_compromises_at_threshold() {
        let m = manager();
        let profile = StealthProfile {
            failure_threshold: 2,
            patch_set_id: "generic".into(),
        };
        let id = m
            .create_session_with_profile(TargetId::Generic, profile)
            .await
            .unwrap();
        m.activate_session(id).await.unwrap();

        let state = m.record_detection(id, event()).await.unwrap();
        assert_eq!(state, SessionState::Active);

        let state = m.record_detection(id, event()).await.unwrap();
        assert_eq!(state, SessionState::Compromised);

        let metrics = m.metrics_snapshot().await;
        assert_eq!(metrics.total_detections, 2);
        // Gauge counts the transition once, not every detection.
        assert_eq!(metrics.compromised_sessions, 1);
    }

    #[tokio::test]
    async fn success_records_countermeasure_once() {
        let m = manager();
        let id = m.create_session(TargetId::Generic).await.unwrap();

        m.record_success(id, Countermeasure::CpuidSpoof).await.unwrap();
        m.record_success(id, Countermeasure::CpuidSpoof).await.unwrap();

        let session = m.get_session(id).await.unwrap();
        assert_eq!(session.success_count, 2);
        assert_eq!(session.active_countermeasures.len(), 1);
        assert_eq!(m.metrics_snapshot().await.total_successes, 2);
    }

    #[tokio::test]
    async fn posture_is_green_for_healthy_session() {
        let m = manager();
        let id = m.create_session(TargetId::Generic).await.unwrap();
        m.activate_session(id).await.unwrap();
        m.record_success(id, Countermeasure::HardwareSpoof)
            .await
            .unwrap();

        let posture = m.posture_for(id).await.unwrap();
        assert_eq!(posture.passed_checks, posture.total_checks);
        assert_eq!(posture.overall_score, 100);
        assert_eq!(posture.posture_level, "GREEN");
    }

    #[tokio::test]
    async fn posture_is_red_for_unactivated_session() {
        let m = manager();
        let id = m.create_session(TargetId::Generic).await.unwrap();

        let posture = m.posture_for(id).await.unwrap();
        assert!(posture.failed_checks > 0);
        assert_eq!(posture.posture_level, "RED");
    }

    #[tokio::test]
    async fn reconcile_metrics_matches_live_sessions() {
        let m = manager();
        let a = m.create_session(TargetId::Generic).await.unwrap();
        let b = m.create_session(TargetId::Generic).await.unwrap();
        m.activate_session(a).await.unwrap();
        m.activate_session(b).await.unwrap();
        m.terminate_session(b).await.unwrap();

        let metrics = m.reconcile_metrics().await;
        assert_eq!(metrics.active_sessions, 1);
        assert_eq!(metrics.compromised_sessions, 0);
    }

    #[tokio::test]
    async fn remove_session_drops_it() {
        let m = manager();
        let id = m.create_session(TargetId::Generic).await.unwrap();
        m.remove_session(id).await.unwrap();

        assert_eq!(m.session_count().await, 0);
        assert!(m.get_session(id).await.is_err());
    }

    #[tokio::test]
    async fn managers_are_isolated() {
        let a = manager();
        let b = manager();
        a.create_session(TargetId::Generic).await.unwrap();

        assert_eq!(a.session_count().await, 1);
        assert_eq!(b.session_count().await, 0);
    }

    #[tokio::test]
    async fn session_ids_are_unique() {
        let m = manager();
        let a = m.create_session(TargetId::Generic).await.unwrap();
        let b = m.create_session(TargetId::Generic).await.unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn error_messages_are_descriptive() {
        let err = StealthError::SessionNotFound {
            id: Uuid::new_v4(),
        };
        assert!(err.to_string().contains("not found"));

        let err = StealthError::InvalidTransition {
            from: "A".into(),
            to: "B".into(),
        };
        assert!(err.to_string().contains('A') && err.to_string().contains('B'));
    }

    #[test]
    fn requirements_skip_timing_by_default() {
        let req = StealthRequirements::default();
        let layers = req.ordered_layers();
        assert!(layers.contains(&StealthLayer::CpuIdMask));
        assert_eq!(
            layers,
            vec![
                StealthLayer::CpuIdMask,
                StealthLayer::HardwareSpoof,
                StealthLayer::ArtifactCleanup,
                StealthLayer::TimingNormalize,
            ]
        );
    }
}