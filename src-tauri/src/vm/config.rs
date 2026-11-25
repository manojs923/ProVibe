use std::path::PathBuf;

use rand::Rng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::hidden_command;
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

/// What kind of product the caller named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// Safe Exam Browser.
    Seb,
    /// Respondus LockDown Browser / Monitor.
    Msb,
}

/// Collapses the several spellings a target arrives as onto one discriminant.
///
/// `engage()` passes a display name (`"respondus lockdown browser"`) while
/// `launch_stealth_vm` passes a short code (`"MSB"`), so both have to land
/// here. SEB and MSB are checked separately — folding `"safeexam"` into the
/// MSB branch sent every Safe Exam Browser run down the Respondus persona.
pub fn classify(target: &str) -> TargetKind {
    let t = target.to_lowercase();

    if t.contains("msb") || t.contains("respondus") || t.contains("lockdown") || t.contains("monitor") {
        return TargetKind::Msb;
    }

    // SEB, "Safe Exam Browser", or anything unrecognised.
    TargetKind::Seb
}

fn is_msb(target: &str) -> bool {
    classify(target) == TargetKind::Msb
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

    // Randomize specs to defeat hardware-uniformity heuristics. The floor is
    // deliberately low: a 2 GB guest starts on a busy 16 GB host, a 6 GB one
    // does not. `PROTOCOL_VM_MEMORY_MB` overrides this at spawn time.
    let memory_mb = if rng.gen_bool(0.6) { 2048 } else { 3072 };
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

/// Push the spoof profile at the VM. Best-effort, and **requires the VM to be
/// powered off** — the BIOS and board serials live in
/// `Msvm_VirtualSystemSettingData` and are committed with
/// `ModifySystemSettings`, not `Set-CimInstance` (whose properties are
/// read-only). Manufacturer and model are not writable through this surface at
/// all, so they stay advisory.
///
/// Runs on the cold path of every fresh provision, so it goes through
/// `hidden_command` — otherwise it flashes a console window on each engage.
pub fn apply_spoof(vm_name: &str, profile: &SpoofProfile) -> Result<(), String> {
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$name = {name}
$mac  = {mac}
$serial = {serial}

Set-VMNetworkAdapter -VMName $name -StaticMacAddress $mac -ErrorAction SilentlyContinue

$vmObj = Get-WmiObject -Namespace root\virtualization\v2 -Class Msvm_ComputerSystem -Filter "ElementName='$name'"
if (-not $vmObj) {{ throw "vm not found: $name" }}
$sd = $vmObj.GetRelated('Msvm_VirtualSystemSettingData') | Where-Object {{ $_.VirtualSystemType -eq 'Microsoft:Hyper-V:System:Realized' }} | Select-Object -First 1
if (-not $sd) {{ throw 'realized setting data not found' }}

$sd.BIOSSerialNumber    = $serial
$sd.BaseBoardSerialNumber = $serial
$sd.ChassisSerialNumber = $serial
$sd.ChassisAssetTag     = 'No Asset Information'

$vmms = Get-WmiObject -Namespace root\virtualization\v2 -Class Msvm_VirtualSystemManagementService
$res = $vmms.ModifySystemSettings($sd.GetText(1))
if ($res.ReturnValue -ne 0 -and $res.ReturnValue -ne 4096) {{
    throw "ModifySystemSettings failed: $($res.ReturnValue)"
}}
"#,
        name = ps_literal(vm_name),
        mac = ps_literal(&profile.mac_address.replace(':', "")),
        serial = ps_literal(&profile.bios_guid),
    );

    let output = hidden_command("powershell.exe")
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