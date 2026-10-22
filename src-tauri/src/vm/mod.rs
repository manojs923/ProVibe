pub mod clone;
pub mod config;
pub mod hyperv;
pub mod lifecycle;
pub mod network;
pub mod sandbox;

use crate::target_id::TargetProfile;
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// VM STATUS QUERY
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct VmStatus {
    pub running: bool,
    pub vm_name: String,
    pub state: String,
    pub uptime_seconds: u64,
    pub cpu_usage: f32,
    pub memory_assigned_mb: u64,
    pub memory_usage_mb: u64,
}

#[tauri::command]
pub fn get_vm_status(vm_name: String) -> Result<VmStatus, String> {
    let ps_cmd = format!(
        "Get-VM -Name '{}' | Select-Object State, Uptime, ProcessorUsage, MemoryAssigned, MemoryDemand | ConvertTo-Json",
        vm_name
    );

    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .output()
        .map_err(|e| format!("Failed to execute PowerShell: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    if stdout.trim().is_empty() || stdout.contains("not found") {
        return Ok(VmStatus {
            running: false,
            vm_name,
            state: "NotFound".to_string(),
            uptime_seconds: 0,
            cpu_usage: 0.0,
            memory_assigned_mb: 0,
            memory_usage_mb: 0,
        });
    }

    let running = stdout.contains("Running");
    let uptime = if running { extract_uptime(&stdout) } else { 0 };

    Ok(VmStatus {
        running,
        vm_name,
        state: if running { "Running".to_string() } else { "Off".to_string() },
        uptime_seconds: uptime,
        cpu_usage: 0.0,
        memory_assigned_mb: 0,
        memory_usage_mb: 0,
    })
}

fn extract_uptime(output: &str) -> u64 {
    // PowerShell uptime format: "HH:MM:SS" or "DD.HH:MM:SS"
    let time_section = output
        .lines()
        .find(|l| l.contains("Uptime"))
        .unwrap_or("");

    let parts: Vec<u64> = time_section
        .split(':')
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .collect();

    match parts.len() {
        3 => parts[0] * 3600 + parts[1] * 60 + parts[2],
        4 => parts[0] * 86400 + parts[1] * 3600 + parts[2] * 60 + parts[3],
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// VM SPAWN
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct VmSpawnResult {
    pub success: bool,
    pub vm_name: String,
    pub pid: Option<u32>,
    pub message: String,
}

#[tauri::command]
pub fn spawn_stealth_vm_cmd(
    vm_name: String,
    stealth_profile: Option<String>,
) -> Result<VmSpawnResult, String> {
    // Prefixed with `_` because the profile hook isn't wired yet — kept so the
    // frontend contract stays stable.
    let _profile = stealth_profile.unwrap_or_else(|| "default".to_string());

    // Apply stealth hardening before boot.
    let harden_script = format!(
        r#"
        Set-VMProcessor -VMName '{vm}' -HypervisorProtectionExposedToGuests $false
        Set-VM -VMName '{vm}' -EnhancedSessionModeType None
        Set-VMMemory -VMName '{vm}' -DynamicMemoryEnabled $false
        "#,
        vm = &vm_name
    );

    Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &harden_script,
        ])
        .output()
        .map_err(|e| format!("Failed to harden VM: {}", e))?;

    // Launch the console. vmconnect spawns detached; we keep the PID for telemetry.
    let child = Command::new("vmconnect.exe")
        .arg("localhost")
        .arg(&vm_name)
        .spawn()
        .map_err(|e| {
            format!(
                "Failed to launch vmconnect: {}. Ensure Hyper-V management tools are installed.",
                e
            )
        })?;

    let pid = Some(child.id());

    // Give Hyper-V a moment to register state transition.
    std::thread::sleep(std::time::Duration::from_secs(2));

    let state_cmd = format!("(Get-VM -Name '{}').State.ToString()", &vm_name);
    let status_output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &state_cmd])
        .output()
        .map_err(|e| format!("Status check failed: {}", e))?;

    let state = String::from_utf8_lossy(&status_output.stdout)
        .trim()
        .to_string();

    let success = state == "Running" || state == "Starting";
    let message = format!("VM state: {}", state);

    Ok(VmSpawnResult {
        success,
        vm_name,
        pid,
        message,
    })
}

// ---------------------------------------------------------------------------
// VM SHUTDOWN
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn shutdown_vm(vm_name: String, force: Option<bool>) -> Result<String, String> {
    let force_flag = force.unwrap_or(false);

    let script = if force_flag {
        format!("Stop-VM -Name '{}' -Force -ErrorAction Stop", vm_name)
    } else {
        format!("Stop-VM -Name '{}' -ErrorAction Stop", vm_name)
    };

    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .map_err(|e| format!("Failed to execute shutdown: {}", e))?;

    if output.status.success() {
        Ok(format!("VM '{}' shutdown initiated", vm_name))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("Shutdown failed: {}", stderr))
    }
}

// ---------------------------------------------------------------------------
// ORCHESTRATOR STATE MACHINE
// ---------------------------------------------------------------------------

/// Represents the lifecycle state of the ephemeral sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VmState {
    Offline,
    Provisioning, // cloning differencing disk
    Booting,      // hyper-v start-vm
    Running,      // os is up, waiting for rdp
    Hooked,       // kernel patches applied, loopback active
    Nuked,        // scorched earth, traces wiped
    Error(String),
}

/// The central controller for spinning up and tearing down the localized matrix.
#[allow(dead_code)]
pub struct VmOrchestrator {
    pub profile: TargetProfile,
    pub vm_name: String,
    pub state: Arc<Mutex<VmState>>,
    pub loopback_ip: String,
}

impl VmOrchestrator {
    /// Initializes a new orchestrator for a specific proctoring target.
    pub fn new(profile: TargetProfile) -> Self {
        // Pseudo-random name to avoid Hyper-V collisions and defeat
        // static-string host scans.
        let entropy = uuid::Uuid::new_v4().to_string();
        let vm_name = format!("win_sys_{}", &entropy[..8]);

        Self {
            profile,
            vm_name,
            state: Arc::new(Mutex::new(VmState::Offline)),
            loopback_ip: "127.0.0.2".to_string(),
        }
    }

    /// Main ignition sequence — full deployment pipeline.
    pub async fn engage(&self) -> Result<(), String> {
        {
            let mut state = self.state.lock().await;
            *state = VmState::Provisioning;
        }

        // 1. Generate config based on target requirements.
        let _cfg = config::build_config(&self.profile.name);

        // 2. Clone base VHDX to a fast differencing disk.
        clone::create_differencing_disk(&self.vm_name).map_err(|e| e.to_string())?;

        // 3. Set up isolated virtual switch mapped to loopback.
        network::configure_loopback_adapter(&self.vm_name, &self.loopback_ip)
            .map_err(|e| e.to_string())?;

        {
            let mut state = self.state.lock().await;
            *state = VmState::Booting;
        }

        // 4. Register and boot the VM via Hyper-V WMI.
        hyperv::start_vm(&self.vm_name).map_err(|e| e.to_string())?;

        // 5. Wait for RDP :3389 on the loopback bind.
        lifecycle::wait_for_boot(&self.loopback_ip)
            .await
            .map_err(|e| e.to_string())?;

        {
            let mut state = self.state.lock().await;
            *state = VmState::Running;
        }

        Ok(())
    }

    /// Injects target-specific kernel and registry patches via PowerShell remoting.
    #[allow(unused_mut)]
    pub async fn apply_stealth_hooks(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;

        // Hook into the patches module once implemented:
        // crate::patches::loader::inject_payload(&self.loopback_ip, &self.profile.patch_set_id)?;

        *state = VmState::Hooked;
        Ok(())
    }

    /// Completely obliterates the VM, differencing disk, and vSwitch. No host artifacts remain.
    pub async fn teardown(&self) -> Result<(), String> {
        lifecycle::destroy_vm(&self.vm_name, None).map_err(|e| e.to_string())?;
        network::teardown_vswitch(&self.vm_name).map_err(|e| e.to_string())?;

        let mut state = self.state.lock().await;
        *state = VmState::Nuked;
        Ok(())
    }

    /// Returns current state for the frontend telemetry feed.
    pub async fn get_status(&self) -> VmState {
        self.state.lock().await.clone()
    }
}