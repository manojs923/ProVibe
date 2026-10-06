// src-tauri/src/stealth/mod.rs

pub mod artifact;
pub mod cpuid;
pub mod hardware;
pub mod timing;

use crate::target_id::matcher::TargetProfile;
use crate::vm::lifecycle::VmHandle;

use std::sync::Arc;
use tokio::sync::RwLock;
use thiserror::Error;

use serde::Serialize;
use std::process::Command;
use sysinfo::{System, SystemExt, ProcessExt};

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

fn check_process_running(signature: &str) -> bool {
    let mut sys = System::new_all();
    sys.refresh_processes();
    sys.processes()
        .iter()
        .any(|(_, p)| p.name().to_lowercase().contains(&signature.to_lowercase()))
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

    // Check 1: Hypervisor services not leaking to host detection tools
    let hyperv_running = check_process_running("vmms");
    checks.push(StealthCheck {
        check_name: "hyperv_management".to_string(),
        passed: true, // VMMS running is fine — it's the Guest that matters
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

    // Check 2: RDP window is available for optical capture
    let rdp_procs = ["mstsc", "vmconnect", "rdpclient"];
    let rdp_found = rdp_procs.iter().any(|p| check_process_running(p));
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

    // Check 3: Proctoring software detection (informational — what's running)
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
        .filter(|sig| check_process_running(sig))
        .map(|s| s.to_string())
        .collect();

    checks.push(StealthCheck {
        check_name: "proctoring_software".to_string(),
        passed: true, // Informational — optical air-gap makes this irrelevant
        status: if running_proctors.is_empty() {
            "No proctoring software detected on Host".to_string()
        } else {
            format!("Active proctoring: {} (Guest-side only — optical gap holds)", running_proctors.join(", "))
        },
        remediation: None,
    });

    // Check 4: Tauri overlay window flags
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

    // Check 5: Network adapter state (loopback tunnel)
    let adapter_result = powershell_scalar(
        "(Get-NetAdapter | Where-Object { $_.Name -match 'Loopback|Protocol' } | Measure-Object).Count",
    );
    let tunnel_ready = adapter_result.parse::<u32>().unwrap_or(0) > 0;
    checks.push(StealthCheck {
        check_name: "loopback_tunnel".to_string(),
        passed: true, // Advisory — tunnel not required for local mode
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

    // Check 6: RDP port 3389 listening
    let port_check = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:3389".parse().map_err(|e| e.to_string())?,
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

    // Check 7: Windows Defender real-time scanning (can flag the overlay binary)
    let defender_result = powershell_scalar(
        "(Get-MpPreference | Select-Object -ExpandProperty DisableRealtimeMonitoring)",
    );
    let defender_off = defender_result.contains("True");
    checks.push(StealthCheck {
        check_name: "defender_realtime".to_string(),
        passed: true, // Informational
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
}

pub type StealthResult<T> = Result<T, StealthError>;

/// Stealth layer identifiers, applied in order.
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

    /// Default application order. Layers can be reordered per target profile.
    pub fn execution_order(&self) -> u8 {
        match self {
            StealthLayer::CpuIdMask => 0,
            StealthLayer::HardwareSpoof => 1,
            StealthLayer::ArtifactCleanup => 2,
            StealthLayer::TimingNormalize => 3,
        }
    }
}

/// Which layers a target profile demands, plus any overrides.
#[derive(Debug, Clone)]
pub struct StealthRequirements {
    /// Layers to apply. If None, apply all layers.
    pub layers: Option<Vec<StealthLayer>>,
    /// Skip timing normalization on targets that don't do timing checks.
    pub skip_timing: bool,
    /// Force hardware spoof even for generic profile.
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
        let detection_vectors = &profile.detection_vectors;

        let skip_timing = !detection_vectors.iter().any(|v| {
            matches!(
                v.kind(),
                crate::target_id::matcher::DetectionVectorKind::TimingAnomaly
            )
        });

        let mut layers: Vec<StealthLayer> = StealthLayer::ALL.to_vec();

        if skip_timing {
            layers.retain(|l| *l != StealthLayer::TimingNormalize);
        }

        if profile.patch_set_id == "generic" {
            // Generic profile: no targeted patches, lean on stealth hard
            layers.push(StealthLayer::TimingNormalize); // generic always includes timing
        }

        Self {
            layers: Some(layers),
            skip_timing,
            force_hardware_spoof: profile.patch_set_id == "generic",
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

/// Tracks which stealth layers succeeded, for rollback and reporting.
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

/// Active stealth session state, updated as layers apply.
#[derive(Debug, Default)]
pub struct StealthSession {
    pub report: RwLock<StealthReport>,
}

/// Arc wrapper so IPC commands can share the session.
pub type SharedStealthSession = Arc<StealthSession>;

impl StealthSession {
    pub fn new(target_id: impl Into<String>) -> Self {
        Self {
            report: RwLock::new(StealthReport {
                target_id: target_id.into(),
                ..Default::default()
            }),
        }
    }

    pub async fn record_success(&self, layer: StealthLayer) {
        let mut guard = self.report.write().await;
        guard.applied_layers.push(layer);
    }

    pub async fn record_failure(&self, layer: StealthLayer, reason: String) {
        let mut guard = self.report.write().await;
        guard.failed_layers.push((layer, reason));
    }

    pub async fn set_duration(&self, ms: u128) {
        let mut guard = self.report.write().await;
        guard.duration_ms = ms;
    }

    pub async fn snapshot(&self) -> StealthReport {
        self.report.read().await.clone()
    }
}

// ---------------------------------------------------------------------------
// Public API entry point
// ---------------------------------------------------------------------------

/// Apply full stealth configuration to a running VM based on target profile.
///
/// This is the primary export of the stealth module. Orchestrates all stealth
/// layers in dependency order:
///
///   1. CPUID hypervisor masking (must happen before VM boots fully — see note)
///   2. Hardware ID spoofing (BIOS/SMBIOS/MAC/disk serials)
///   3. VM artifact cleanup (registry keys, guest services, device names)
///   4. Timing normalization (QPC/TSC/TickCount consistency)
///
/// NOTE: If the VM is already running, CPUID masking is applied as a
/// processor setting + live update where Hyper-V supports it. For cold VMs,
/// the mask is applied pre-boot and requires a restart flag.
///
/// Returns a StealthReport detailing exactly which layers landed.
pub async fn apply_stealth_config(
    vm_handle: &VmHandle,
    target_profile: &TargetProfile,
) -> StealthResult<StealthReport> {
    let start = std::time::Instant::now();

    if !vm_handle.is_alive().await {
        return Err(StealthError::InvalidHandle(vm_handle.vm_name().to_string()));
    }

    let requirements = StealthRequirements::from_profile(target_profile);
    let layers = requirements.ordered_layers();

    if layers.is_empty() {
        return Err(StealthError::MissingConfig(format!(
            "profile '{}' produced empty stealth layer set",
            target_profile.target_id
        )));
    }

    let session = StealthSession::new(&target_profile.target_id);
    let total = layers.len();

    for layer in layers {
        let result = match layer {
            StealthLayer::CpuIdMask => cpuid::mask_hypervisor(vm_handle).await,

            StealthLayer::HardwareSpoof => {
                let persona = hardware::select_persona(target_profile);
                hardware::apply_spoof(vm_handle, &persona).await
            }

            StealthLayer::ArtifactCleanup => artifact::cleanup_vm_artifacts(vm_handle).await,

            StealthLayer::TimingNormalize => {
                let profile_timing = timing::TimingProfile::for_target(target_profile);
                timing::normalize(vm_handle, &profile_timing).await
            }
        };

        match result {
            Ok(()) => session.record_success(layer).await,
            Err(e) => {
                tracing::error!(
                    layer = layer.as_str(),
                    target = %target_profile.target_id,
                    error = %e,
                    "stealth layer failed"
                );
                session.record_failure(layer, e.to_string()).await;
            }
        }

        // Bail if VM died mid-application
        if !vm_handle.is_alive().await {
            session.set_duration(start.elapsed().as_millis()).await;
            return Err(StealthError::VmLost);
        }
    }

    session.set_duration(start.elapsed().as_millis()).await;
    let report = session.snapshot().await;

    if report.failed_layers.is_empty() {
        tracing::info!(
            target = %target_profile.target_id,
            layers = ?report.applied_layers.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            duration_ms = report.duration_ms,
            "stealth config fully applied"
        );
        Ok(report)
    } else if report.applied_layers.len() < total {
        Err(StealthError::PartialApplication {
            applied: report.applied_layers.len(),
            total,
        })
    } else {
        // Some failed but enough applied to proceed with warnings
        tracing::warn!(
            target = %target_profile.target_id,
            failed = ?report.failed_layers,
            "stealth config partially applied, proceeding with degraded coverage"
        );
        Ok(report)
    }
}

/// Rollback stealth changes — restores VM to pre-stealth state.
/// Used when destroying VMs that failed mid-pipeline, to keep the base
/// differencing disk clean.
pub async fn rollback_stealth_config(vm_handle: &VmHandle) -> StealthResult<()> {
    artifact::restore_artifacts(vm_handle).await?;
    cpuid::unmask_hypervisor(vm_handle).await?;
    Ok(())
}

/// Quick pre-flight check: does the host environment support the stealth
/// layers this profile needs? Run before spawning VM to fail fast.
pub fn validate_host_support(target_profile: &TargetProfile) -> StealthResult<()> {
    let requirements = StealthRequirements::from_profile(target_profile);

    for layer in requirements.ordered_layers() {
        let supported = match layer {
            StealthLayer::CpuIdMask => cpuid::host_supports_masking(),
            StealthLayer::HardwareSpoof => hardware::host_supports_spoofing(),
            StealthLayer::ArtifactCleanup => true, // registry work always available
            StealthLayer::TimingNormalize => timing::host_supports_timing_patch(),
        };

        if !supported {
            return Err(StealthError::MissingConfig(format!(
                "host does not support required stealth layer '{}'",
                layer.as_str()
            )));
        }
    }

    Ok(())
}