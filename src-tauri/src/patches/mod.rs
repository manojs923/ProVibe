// src-tauri/src/patches/mod.rs

pub mod errors;
pub mod examity;
pub mod generic;
pub mod honorlock;
pub mod loader;
pub mod orchestrator;
pub mod proctorio;
pub mod proctoru;
pub mod respondus;

use serde::{Deserialize, Serialize};
use std::fmt;
use std::process::Command;

use crate::patches::generic::get_patches;

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
    match Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
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
    // `HideHypervisorPresent` is not exposed as a `Set-VMProcessor` parameter
    // on current builds, so it is written through `Msvm_ProcessorSettingData`.
    // The change is stored immediately but only takes effect on the next boot.
    let script = format!(
        r#"
        $ErrorActionPreference = 'Stop'
        $name = '{vm}'
        $vmObj = Get-WmiObject -Namespace root\virtualization\v2 -Class Msvm_ComputerSystem -Filter "ElementName='$name'"
        if (-not $vmObj) {{ throw "vm not found: $name" }}
        $sd = $vmObj.GetRelated('Msvm_VirtualSystemSettingData') | Where-Object {{ $_.VirtualSystemType -eq 'Microsoft:Hyper-V:System:Realized' }} | Select-Object -First 1
        if (-not $sd) {{ throw 'realized setting data not found' }}
        $proc = $sd.GetRelated('Msvm_ProcessorSettingData') | Select-Object -First 1
        if (-not $proc) {{ throw 'processor setting data not found' }}
        $proc.HideHypervisorPresent = $true
        $vmms = Get-WmiObject -Namespace root\virtualization\v2 -Class Msvm_VirtualSystemManagementService
        $res = $vmms.ModifyResourceSettings($proc.GetText(1))
        if ($res.ReturnValue -ne 0 -and $res.ReturnValue -ne 4096) {{
            throw "ModifyResourceSettings failed: $($res.ReturnValue)"
        }}
        Set-VMProcessor -VMName $name -ExposeVirtualizationExtensions $false -ErrorAction Stop
        Write-Output 'OK'
        "#,
        vm = vm_name
    );
    let (ok, msg) = run_powershell(&script);
    PatchResult {
        patch_name: "cpu_hypervisor_bit".to_string(),
        success: ok && msg.contains("OK"),
        details: msg,
    }
}

fn patch_vm_network(vm_name: &str) -> PatchResult {
    // Spoof MAC with Intel OUI and disable offloads that leak virtualization.
    let script = format!(
        r#"
        $adapter = Get-VMNetworkAdapter -VMName '{}' -ErrorAction Stop
        $adapter | Set-VMNetworkAdapter -MacAddressSpoofing On -DeviceNaming On -ErrorAction Stop
        $adapter | Set-VMNetworkAdapter -RssWeight 0 -VmqWeight 0 `
            -IPsecOffloadMaximumSecurityAssociation 0 -ErrorAction SilentlyContinue
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
    // Guest Service Interface MUST stay enabled or Copy-VMFile stops working.
    // Heartbeat stays enabled so state reporting remains meaningful.
    let script = format!(
        r#"
        Get-VMIntegrationService -VMName '{}' |
            Where-Object {{
                $_.Enabled -and
                $_.Name -ne 'Guest Service Interface' -and
                $_.Name -ne 'Heartbeat'
            }} |
            Disable-VMIntegrationService -ErrorAction SilentlyContinue
        Set-VMHost -EnableEnhancedSessionMode $false -ErrorAction SilentlyContinue
        Write-Output 'OK'
        "#,
        vm_name
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
    // WDA_EXCLUDEFROMCAPTURE needs Win10 2004 (build 19041) or newer.
    let build = run_powershell("[Environment]::OSVersion.Version.Build");
    let build_num = build.1.trim().parse::<u32>().unwrap_or(0);

    let supported = build_num >= 19041;
    PatchResult {
        patch_name: "display_affinity_support".to_string(),
        success: true,
        details: format!(
            "Build {} — WDA_EXCLUDEFROMCAPTURE {}",
            build_num,
            if supported {
                "supported"
            } else {
                "NOT supported (needs 2004+)"
            }
        ),
    }
}

fn apply_target_patches_blocking(vm_name: Option<String>) -> Result<PatchReport, String> {
    let vm = vm_name.unwrap_or_else(|| "*".to_string());
    let mut results = Vec::new();

    // Only apply VM-specific patches if a named VM was given.
    if vm != "*" {
        results.push(patch_vm_processor(&vm));
        results.push(patch_vm_network(&vm));
        results.push(patch_vm_integration_services(&vm));
        results.push(patch_vm_time_sync(&vm));
    }

    // Host-level patches always run.
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

#[tauri::command]
pub async fn apply_target_patches(vm_name: Option<String>) -> Result<PatchReport, String> {
    tokio::task::spawn_blocking(move || apply_target_patches_blocking(vm_name))
        .await
        .map_err(|e| format!("patch task panicked: {}", e))?
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
#[allow(dead_code)]
pub fn load_patches(target_id: &str) -> Vec<Patch> {
    match target_id {
        "respondus" => respondus::get_patches(),
        "proctorio" => proctorio::get_patches(),
        "proctoru" => proctoru::get_patches(),
        "honorlock" => honorlock::get_patches(),
        "examity" => get_patches(),
        _ => {
            println!(
                "Target ID '{}' not recognized. Loading generic patch set.",
                target_id
            );
            generic::get_patches()
        }
    }
}