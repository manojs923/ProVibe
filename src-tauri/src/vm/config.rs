use serde::{Deserialize, Serialize};
use uuid::Uuid;
use rand::Rng;

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

pub fn build_config(target_id: &str) -> VmConfig {
    let mut rng = rand::thread_rng();
    
    // unique naming to avoid hyper-v collisions
    let suffix = &Uuid::new_v4().to_string()[..8];
    let vm_name = format!("win11_sys_{}", suffix);

    // randomize specs to defeat hardware uniformity heuristics
    let memory_mb = if rng.gen_bool(0.6) { 8192 } else { 16384 }; 
    let cpu_cores = if rng.gen_bool(0.7) { 4 } else { 8 };

    // generate a believable intel oui mac address (e4:42:a6) instead of hyper-v default (00:15:5d)
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
        "SEB" | _ => VmConfig {
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