use std::path::PathBuf;
use std::process::Command;

use rand::Rng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::util::ps_literal;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub vm_name: String,
    pub memory_mb: u32,
    pub cpu_cores: u8,
    pub disk_size_gb: u32,
    pub network_isolation: NetworkMode,
    pub spoof_profile: SpoofProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NetworkMode {
    AirGapped,
    LoopbackOnly,
    NatWithDnsFilter, // for when the lockdown browser needs to phone home
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoofProfile {
    pub mac_address: String,
    pub bios_guid: String,
    pub manufacturer: String,
    pub model: String,
}

// ---------------------------------------------------------------------------
// Stealth subsystem configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct StealthConfig {
    pub guest_vm_name: String,
    pub backup_directory: PathBuf,
    pub guest_restore_root: Option<PathBuf>,
    pub skip_backup: bool,
    pub verify_after_cleanup: bool,
}

impl Default for StealthConfig {
    fn default() -> Self {
        Self {
            guest_vm_name: String::new(),
            backup_directory: std::env::temp_dir().join("protocol_stealth_backups"),
            guest_restore_root: None,
            skip_backup: false,
            verify_after_cleanup: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Target normalisation
// ---------------------------------------------------------------------------

/// Collapses the several spellings a target arrives as — `"MSB"`, `"SEB"`,
/// `"Respondus LockDown Browser"` — onto one discriminant. `engage()` passes a
/// display name while `launch_stealth_vm` passes a short code, so both have to
/// land here.
fn is_msb(target: &str) -> bool {
    let t = target.to_lowercase();
    t.contains("msb")
        || t.contains("respondus")
        || t.contains("lockdown")
        || t.contains("safeexam")
}

// ---------------------------------------------------------------------------
// Profile-derived VM configuration
// ---------------------------------------------------------------------------

/// Build a config for `target` with a freshly generated VM name.
pub fn build_config(target_id: &str) -> VmConfig {
    build_config_named(target_id, &format!("win11_sys_{}", &Uuid::new_v4().to_string()[..8]))
}

/// Build a config pinned to a caller-supplied name, so the orchestrator's name
/// is the one that reaches Hyper-V instead of being discarded.
pub fn build_config_named(target_id: &str, vm_name: &str) -> VmConfig {
    let mut rng = rand::thread_rng();

    // Randomize specs to defeat hardware-uniformity heuristics.
    let memory_mb = if rng.gen_bool(0.6) { 8192 } else { 16384 };
    let cpu_cores = if rng.gen_bool(0.7) { 4 } else { 8 };

    // Believable Intel OUI (E4:42:A6) instead of Hyper-V's 00:15:5D.
    let mac = format!(
        "E4:42:A6:{:02X}:{:02X}:{:02X}",
        rng.gen::<u8>(),
        rng.gen::<u8>(),
        rng.gen::<u8>()
    );
    let bios_guid = Uuid::new_v4().to_string();

    if is_msb(target_id) {
        VmConfig {
            vm_name: vm_name.to_string(),
            memory_mb,
            cpu_cores,
            disk_size_gb: 128,
            network_isolation: NetworkMode::NatWithDnsFilter,
            spoof_profile: SpoofProfile {
                mac_address: mac,
                bios_guid,
                manufacturer: "Dell Inc.".to_string(),
                model: "OptiPlex 7090".to_string(),
            },
        }
    } else {
        VmConfig {
            vm_name: vm_name.to_string(),
            memory_mb,
            cpu_cores,
            disk_size_gb: 64,
            network_isolation: NetworkMode::LoopbackOnly,
            spoof_profile: SpoofProfile {
                mac_address: mac,
                bios_guid,
                manufacturer: "LENOVO".to_string(),
                model: "ThinkCentre M70q".to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Spoof application
// ---------------------------------------------------------------------------

/// Push the spoof profile at the VM. Best-effort: the MAC always lands, the
/// BIOS serial only on hosts where `Set-CimInstance` is allowed against
/// `Msvm_BIOSElement`. Manufacturer/model are not writable through the WMI
/// surface at all, so they stay advisory here.
pub fn apply_spoof(vm_name: &str, profile: &SpoofProfile) -> Result<(), String> {
    let script = format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
$name = {name}
Set-VMNetworkAdapter -VMName $name -MacAddress {mac}

$vm = Get-CimInstance -Namespace root\virtualization\v2 -Class Msvm_ComputerSystem |
      Where-Object {{ $_.ElementName -eq $name }}
$sd = Get-CimAssociatedInstance -InputObject $vm -ResultClassName Msvm_VirtualSystemSettingData |
      Where-Object {{ $_.VirtualSystemType -eq 'Microsoft:Hyper-V:System:Realized' }} |
      Select-Object -First 1
$bios = Get-CimAssociatedInstance -InputObject $sd -ResultClassName Msvm_BIOSElement
if ($bios) {{ $bios | Set-CimInstance -Property @{{ SerialNumber = {serial} }} }}
"#,
        name = ps_literal(vm_name),
        mac = ps_literal(&profile.mac_address.replace(':', "")),
        serial = ps_literal(&profile.bios_guid),
    );

    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()
        .map_err(|e| format!("spoof invocation failed: {}", e))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}