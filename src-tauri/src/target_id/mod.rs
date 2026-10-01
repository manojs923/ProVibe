pub mod scanner;
pub mod profiles;
pub mod matcher;

use serde::{Deserialize, Serialize};
use std::fmt;

/// represents a specific lockdown browser or proctoring system
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TargetId {
    Respondus,
    Proctorio,
    ProctorU,
    Honorlock,
    Examity,
    Generic, // fallback for unknown glowie tech
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Formatter {
        match self {
            TargetId::Respondus => write!(f, "Respondus LockDown Browser"),
            TargetId::Proctorio => write!(f, "Proctorio"),
            TargetId::ProctorU => write!(f, "ProctorU"),
            TargetId::Honorlock => write!(f, "Honorlock"),
            TargetId::Examity => write!(f, "Examity"),
            TargetId::Generic => write!(f, "Generic/Unknown Proctor"),
        }
    }
}

/// detection vectors used by the matcher to identify the target
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionVector {
    pub process_names: Vec<String>,
    pub registry_keys: Vec<String>,
    pub service_names: Vec<String>,
    pub window_classes: Vec<String>,
}

/// defines how the vm should be configured to evade this specific target
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VMConfigOverride {
    pub requires_internet: bool,
    pub min_ram_gb: u32,
    pub min_cores: u32,
    pub strict_timing_spoof: bool,
}

/// the complete profile for a target, defining how to detect it and how to patch the vm for it
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetProfile {
    pub target_id: TargetId,
    pub display_name: String,
    pub detection_vectors: DetectionVector,
    pub patch_set_id: String,
    pub vm_config_override: Option<VMConfigOverride>,
}

/// attempts to identify a running proctoring system on the host os.
/// warning: if the architecture is purely vm-based, this will only detect
/// if the user accidentally launched the exam on the bare metal host.
pub fn identify_target() -> Option<TargetProfile> {
    // scan host processes, registry, and services
    let artifacts = scanner::scan_host_artifacts();
    
    if artifacts.is_empty() {
        // host is clean. no proctoring software running locally.
        return None;
    }

    // pass artifacts to the fuzzy matcher
    matcher::match_artifacts(artifacts)
}

/// retrieves the static profile for a known target.
/// use this when the user explicitly selects the target in the c2 panel
/// prior to spinning up the stealth vm.
pub fn get_profile(id: TargetId) -> TargetProfile {
    profiles::fetch_profile(id)
}

/// returns a list of all supported target profiles for the frontend ui
pub fn get_all_supported_profiles() -> Vec<TargetProfile> {
    profiles::fetch_all()
}