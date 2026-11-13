use std::path::PathBuf;

use rand::Rng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

/// Settings for the guest-artifact cleaner and stealth layer runner.
///
/// `guest_vm_name` is the only field that must be right for a run; everything
/// else has a sane default so callers can build one from a VM handle with
/// `..Default::default()`.
#[derive(Debug, Clone)]
pub struct StealthConfig {
    /// Guest VM the artifact cleaner operates against.
    pub guest_vm_name: String,
    /// Host directory that holds artifact backups before deletion.
    pub backup_directory: PathBuf,
    /// Where restores land inside the guest when the original path is unknown.
    /// `None` falls back to `C:\Restored\`.
    pub guest_restore_root: Option<PathBuf>,
    /// Skip the host-side backup step and delete artifacts outright.
    pub skip_backup: bool,
    /// Re-scan after a cleanup pass to confirm the guest is clear.
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
// Profile-derived VM configuration
// ---------------------------------------------------------------------------

pub fn build_config(target_id: &str) -> VmConfig {
    let mut rng = rand::thread_rng();

    // Unique naming to avoid Hyper-V collisions.
    let uuid = Uuid::new_v4().to_string();
    let vm_name = format!("win11_sys_{}", &uuid[..8]);

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

    match target_id {
        "MSB" => VmConfig {
            vm_name,
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
        },
        _ => VmConfig {
            vm_name,
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
        },
    }
}