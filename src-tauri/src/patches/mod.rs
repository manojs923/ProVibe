// src-tauri/src/patches/mod.rs

pub mod loader;
pub mod respondus;
pub mod proctorio;
pub mod proctoru;
pub mod honorlock;
pub mod examity;
pub mod generic;

use serde::{Deserialize, Serialize};
use std::fmt;
use std::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct PatchResult {
    pub patch_name: String,
    pub success: bool,
    pub details: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PatchReport {
    pub total_patches: usize,
    pub successful: usize,
    pub failed: usize,
    pub results: Vec<PatchResult>,
    pub requires_reboot: bool,
}

fn run_powershell(script: &str) -> (bool, String) {
    match Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if output.status.success() {
                (true, stdout)
            } else {
                (false, if stderr.is_empty() { stdout } else { stderr })
            }
        }
        Err(e) => (false, format!("Spawn failed: {}", e)),
    }
}

fn patch_vm_processor(vm_name: &str) -> PatchResult {
    let script = format!(
        r#"
        Set-VMProcessor -VMName '{}' -HypervisorProtectionExposedToGuests $false -ErrorAction Stop
        Set-VMProcessor -VMName '{}' -ExposeVirtualizationExtensions $false -ErrorAction Stop
        Write-Output 'OK'
        "#,
        vm_name, vm_name
    );
    let (ok, msg) = run_powershell(&script);
    PatchResult {
        patch_name: "cpu_hypervisor_bit".to_string(),
        success: ok && msg.contains("OK"),
        details: msg,
    }
}

fn patch_vm_network(vm_name: &str) -> PatchResult {
    // Spoof MAC with Intel OUI and disable RSS/VMQ offloads that leak virtualization
    let script = format!(
        r#"
        $adapter = Get-VMNetworkAdapter -VMName '{}' -ErrorAction Stop
        $adapter | Set-VMNetworkAdapter -MacAddressSpoofing On -DeviceNaming On -ErrorAction Stop
        $adapter | Set-VMNetworkAdapter -RssWeight 0 -VmqWeight 0 -IPsecOffloadMaximumSA 0 -ErrorAction SilentlyContinue
        Write-Output 'OK'
        "#,
        vm_name
    );
    let (ok, msg) = run_powershell(&script);
    PatchResult {
        patch_name: "network_offload_disable".to_string(),
        success: ok && msg.contains("OK"),
        details: msg,
    }
}

fn patch_vm_integration_services(vm_name: &str) -> PatchResult {
    let script = format!(
        r#"
        Get-VMIntegrationService -VMName '{}' | Where-Object {{ $_.Enabled -eq $true -and $_.Id -ne 'Microsoft:VirtualMachine\Status' }} | Disable-VMIntegrationService -ErrorAction SilentlyContinue
        Set-VM -VMName '{}' -EnhancedSessionModeType None -ErrorAction Stop
        Write-Output 'OK'
        "#,
        vm_name, vm_name
    );
    let (ok, msg) = run_powershell(&script);
    PatchResult {
        patch_name: "integration_services_off".to_string(),
        success: ok && msg.contains("OK"),
        details: msg,
    }
}

fn patch_vm_time_sync(vm_name: &str) -> PatchResult {
    let script = format!(
        r#"
        $ts = Get-VMIntegrationService -VMName '{}' -Name 'Time Synchronization' -ErrorAction SilentlyContinue
        if ($ts) {{ Disable-VMIntegrationService -InputObject $ts -ErrorAction SilentlyContinue }}
        Write-Output 'OK'
        "#,
        vm_name
    );
    let (ok, msg) = run_powershell(&script);
    PatchResult {
        patch_name: "time_sync_disabled".to_string(),
        success: ok,
        details: msg,
    }
}

fn patch_host_display_affinity() -> PatchResult {
    // Ensure WDA_EXCLUDEFROMCAPTURE is available (Win10 2004+)
    let build = run_powershell("[Environment]::OSVersion.Version.Build");
    let build_num = build
        .1
        .trim()
        .parse::<u32>()
        .unwrap_or(0);

    let supported = build_num >= 19041;
    PatchResult {
        patch_name: "display_affinity_support".to_string(),
        success: true,
        details: format!(
            "Build {} — WDA_EXCLUDEFROMCAPTURE {}",
            build_num,
            if supported { "supported" } else { "NOT supported (needs 2004+)" }
        ),
    }
}

#[tauri::command]
pub fn apply_target_patches(vm_name: Option<String>) -> Result<PatchReport, String> {
    let vm = vm_name.unwrap_or_else(|| "*".to_string());
    let mut results = Vec::new();

    // Only apply VM-specific patches if a named VM was given
    if vm != "*" {
        results.push(patch_vm_processor(&vm));
        results.push(patch_vm_network(&vm));
        results.push(patch_vm_integration_services(&vm));
        results.push(patch_vm_time_sync(&vm));
    }

    // Host-level patches always run
    results.push(patch_host_display_affinity());

    let successful = results.iter().filter(|r| r.success).count();
    let failed = results.len() - successful;

    Ok(PatchReport {
        total_patches: results.len(),
        successful,
        failed,
        results,
        requires_reboot: failed > 0,
    })
}

/// Represents a specific patch to be applied inside the VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Patch {
    pub id: String,
    pub name: String,
    pub description: String,
    pub patch_type: PatchType,
    pub payload: String, // PowerShell script, registry path, or binary payload depending on type
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PatchType {
    RegistryMod,
    ServiceDisable,
    DriverInject,
    WmiHide,
    EtwSuppress,
    SmbiosSpoof,
}

impl fmt::Display for PatchType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PatchType::RegistryMod => write!(f, "Registry Modification"),
            PatchType::ServiceDisable => write!(f, "Service Disablement"),
            PatchType::DriverInject => write!(f, "Driver Injection"),
            PatchType::WmiHide => write!(f, "WMI Namespace Hiding"),
            PatchType::EtwSuppress => write!(f, "ETW Provider Suppression"),
            PatchType::SmbiosSpoof => write!(f, "SMBIOS Spoofing"),
        }
    }
}

/// Loads the appropriate patch set based on the target ID.
/// This maps the `patch_set_id` from the target profile to concrete implementations.
pub fn load_patches(target_id: &str) -> Vec<Patch> {
    match target_id {
        "respondus" => respondus::get_patches(),
        "proctorio" => proctorio::get_patches(),
        "proctoru" => proctoru::get_patches(),
        "honorlock" => honorlock::get_patches(),
        "examity" => examity::get_patches(),
        _ => {
            // Fallback to generic stealth patches if target is unknown or unspecified
            println!("Target ID '{}' not recognized. Loading generic patch set.", target_id);
            generic::get_patches()
        }
    }
}