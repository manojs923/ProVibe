pub mod clone;
pub mod config;
pub mod hyperv;
pub mod lifecycle;
pub mod network;
pub mod sandbox;

use crate::target_id::TargetProfile;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::process::Command;


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
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Get-VM -Name '{}' | Select-Object State, Uptime, ProcessorUsage, MemoryAssigned, MemoryDemand | ConvertTo-Json",
                vm_name
            ),
        ])
        .output()
        .map_err(|e| format!("Failed to execute PowerShell: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

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

    // Parse the JSON-like PowerShell output
    let running = stdout.contains("Running");
    let uptime = if running {
        extract_uptime(&stdout)
    } else {
        0
    };

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

    let parts: Vec<&str> = time_section
        .split(':')
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .collect();

    match parts.len() {
        3 => parts[0] * 3600 + parts[1] * 60 + parts[2],
        4 => parts[0] * 86400 + parts[1] * 3600 + parts[2] * 60 + parts[3],
        _ => 0,
    }
}


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
    let profile = stealth_profile.unwrap_or_else(|| "default".to_string());

    // Apply stealth configuration before starting
    let harden_script = format!(
        r#"
        Set-VMProcessor -VMName '{vm}' -HypervisorProtectionExposedToGuests $false
        Set-VM -VMName '{vm}' -EnhancedSessionModeType None
        Set-VMMemory -VMName '{vm}' -DynamicMemoryEnabled $false
        "#,
        vm = vm_name
    );

    Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &harden_script])
        .output()
        .map_err(|e| format!("Failed to harden VM: {}", e))?;

    // Start VM with vmconnect (opens console window)
    let child = Command::new("vmconnect.exe")
        .arg("localhost")
        .arg(&vm_name)
        .spawn()
        .map_err(|e| format!("Failed to launch vmconnect: {}. Ensure Hyper-V management tools are installed.", e))?;

    // Wait briefly for VM to register as starting
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Verify it's starting
    let status_output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-VM -Name '{}').State.ToString()", vm_name),
        ])
        .output()
        .map_err(|e| format!("Status check failed: {}", e))?;

    let state = String::from_utf8_lossy(&status_output.stdout).trim().to_string();

    Ok(VmSpawnResult {
        success: state == "Running" || state == "Starting",
        vm_name,
        pid: child.id(),
        message: format!("VM state: {}", state),
    })
}

#[tauri::command]
pub fn shutdown_vm(vm_name: String, force: Option<bool>) -> Result<String, String> {
    let force_flag = force.unwrap_or(false);

    let (cmd, verb) = if force_flag {
        ("Stop-VM", "-Force")
    } else {
        ("Stop-VM", "")
    };

    let script = format!(
        "{} -Name '{}' {} -ErrorAction Stop",
        cmd, vm_name, verb
    );

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

/// represents the lifecycle state of the ephemeral sandbox
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

/// the central controller for spinning up and tearing down the localized matrix.
pub struct VmOrchestrator {
    pub profile: TargetProfile,
    pub vm_name: String,
    pub state: Arc<Mutex<VmState>>,
    pub loopback_ip: String,
}

impl VmOrchestrator {
    /// initializes a new orchestrator for a specific proctoring target
    pub fn new(profile: TargetProfile) -> Self {
        // generate a pseudo-random name to avoid hyper-v collisions 
        // and avoid static string detection by host-level scans
        let entropy = uuid::Uuid::new_v4().to_string();
        let vm_name = format!("win_sys_{}", &entropy[..8]);

        Self {
            profile,
            vm_name,
            state: Arc::new(Mutex::new(VmState::Offline)),
            loopback_ip: "127.0.0.2".to_string(), // hardcoded internal loopback route
        }
    }

    /// the main ignition sequence. executes the full deployment pipeline.
    pub async fn engage(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        *state = VmState::Provisioning;
        
        // 1. generate config based on target requirements
        let _cfg = config::build_config(&self.profile.name); // Use whatever the string field is
        
        // 2. clone base vhdx to a fast differencing disk
        clone::create_differencing_disk(&self.vm_name).map_err(|e| e.to_string())?;
        
        // 3. setup isolated virtual switch mapped to loopback
        network::configure_loopback_adapter(&self.vm_name, &self.loopback_ip).map_err(|e| e.to_string())?;

        *state = VmState::Booting;
        
        // 4. register and boot the vm via hyper-v wmi
        hyperv::start_vm(&self.vm_name).map_err(|e| e.to_string())?;
        
        // 5. wait for rdp port 3389 to become responsive on 127.0.0.2
        lifecycle::wait_for_boot(&self.loopback_ip).await.map_err(|e| e.to_string())?;

        *state = VmState::Running;

        Ok(())
    }

    /// injects the target-specific kernel and registry patches via powershell remoting
    pub async fn apply_stealth_hooks(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        
        // this would call into the `patches/` module we'll build later
        // crate::patches::loader::inject_payload(&self.loopback_ip, &self.profile.patch_set_id)?;

        *state = VmState::Hooked;
        Ok(())
    }

    /// completely obliterates the vm, differencing disk, and vswitch. leaves no host artifacts.
    pub async fn teardown(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        
        lifecycle::destroy_vm(&self.vm_name, None).map_err(|e| e.to_string())?;
        network::teardown_vswitch(&self.vm_name).map_err(|e| e.to_string())?;
        
        *state = VmState::Nuked;
        Ok(())
    }

    /// returns the current state for the frontend telemetry feed
    pub async fn get_status(&self) -> VmState {
        self.state.lock().await.clone()
    }
}