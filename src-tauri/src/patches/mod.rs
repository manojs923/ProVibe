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