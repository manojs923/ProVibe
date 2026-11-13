use serde::{Deserialize, Serialize};
use sysinfo::{System, Process};
use std::fmt;

use crate::app_handle::AppHandle;
use self::scanner::scan_host_artifacts;
use self::matcher::match_artifacts;
pub use self::profiles::{fetch_profile, fetch_all};
use tokio::task;

pub mod scanner;
pub mod matcher;
pub mod profiles;

#[derive(Debug, Clone, Serialize)]
pub struct LockdownDetection {
    pub detected: bool,
    pub process_name: String,
    pub pid: u32,
    pub window_title: Option<String>,
    pub detection_type: LockdownType,
}

#[derive(Debug, Clone, Serialize)]
pub enum LockdownType {
    Respondus,
    SafeExamBrowser,
    Honorlock,
    Proctorio,
    ProctorU,
    Examity,
    TestInvite,
    Unknown,
}

const LOCKDOWN_SIGNATURES: &[(&str, LockdownType)] = &[
    ("lockdownbrowser", LockdownType::Respondus),
    ("respondus", LockdownType::Respondus),
    ("safeexambrowser", LockdownType::SafeExamBrowser),
    ("seb", LockdownType::SafeExamBrowser),
    ("honorlock", LockdownType::Honorlock),
    ("proctorio", LockdownType::Proctorio),
    ("proctoru", LockdownType::ProctorU),
    ("examity", LockdownType::Examity),
    ("testinvite", LockdownType::TestInvite),
];

#[tauri::command]
pub fn scan_for_lockdown_browsers() -> Vec<LockdownDetection> {
    let mut sys = System::new_all();
    sys.refresh_processes();

    let mut detections = Vec::new();

    for (_pid, process) in sys.processes() {
        let name = process.name().to_lowercase();

        for (signature, ld_type) in LOCKDOWN_SIGNATURES {
            if name.contains(signature) {
                detections.push(LockdownDetection {
                    detected: true,
                    process_name: process.name().to_string(),
                    pid: process.pid().as_u32(),
                    window_title: process.cmd().first().map(|s| s.to_string()),
                    detection_type: ld_type.clone(),
                });
                break;
            }
        }
    }

    if detections.is_empty() {
        detections.push(LockdownDetection {
            detected: false,
            process_name: String::new(),
            pid: 0,
            window_title: None,
            detection_type: LockdownType::Unknown,
        });
    }

    detections
}

pub async fn start_detection_loop(app_handle: &AppHandle) {
    println!("Starting detection loop...");

    // Simulate some initialization logic
    let artifacts = scan_host_artifacts();
    let profiles = fetch_all();

    // Spawn a task to handle the detection logic
    task::spawn(async move {
        for profile in profiles {
            if let Some(matched_artifact) = match_artifacts(artifacts.clone()) {
                println!("Matched artifact: {:?}", matched_artifact);
                let _ = fetch_profile(matched_artifact.target_id.clone());
            }
        }

        println!("Detection loop completed.");
    });

    println!("Detection loop started.");
}

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

impl TargetId {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetId::Examity => "examity",
            TargetId::ProctorU => "proctoru",
            TargetId::Proctorio => "proctorio",
            TargetId::Honorlock => "honorlock",
            TargetId::Respondus => "respondus",
            TargetId::Generic => "generic",
        }
    }
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), std::fmt::Error> {
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

/// enum representing the kind of detection vector
/// used by stealth module to determine what evasion measures are needed
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DetectionVectorKind {
    ProcessPresent,
    RegistryKey,
    ServiceRunning,
    WindowClass,
    TimingAnomaly,
}

/// detection vectors used by the matcher to identify the target
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionVector {
    pub process_names: Vec<String>,
    pub registry_keys: Vec<String>,
    pub service_names: Vec<String>,
    pub window_classes: Vec<String>,
}

impl DetectionVector {
    /// returns all detection vector kinds that this vector covers
    pub fn kinds(&self) -> Vec<DetectionVectorKind> {
        let mut kinds = Vec::new();
        if !self.process_names.is_empty() {
            kinds.push(DetectionVectorKind::ProcessPresent);
        }
        if !self.registry_keys.is_empty() {
            kinds.push(DetectionVectorKind::RegistryKey);
        }
        if !self.service_names.is_empty() {
            kinds.push(DetectionVectorKind::ServiceRunning);
        }
        if !self.window_classes.is_empty() {
            kinds.push(DetectionVectorKind::WindowClass);
        }
        kinds
    }

    /// checks if this vector contains a specific kind
    pub fn has_kind(&self, kind: DetectionVectorKind) -> bool {
        match kind {
            DetectionVectorKind::ProcessPresent => !self.process_names.is_empty(),
            DetectionVectorKind::RegistryKey => !self.registry_keys.is_empty(),
            DetectionVectorKind::ServiceRunning => !self.service_names.is_empty(),
            DetectionVectorKind::WindowClass => !self.window_classes.is_empty(),
            DetectionVectorKind::TimingAnomaly => false,
        }
    }
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
    let artifacts = scanner::scan_host_artifacts();

    if artifacts.is_empty() {
        return None;
    }

    matcher::match_artifacts(artifacts)
}

/// retrieves the static profile for a known target.
pub fn get_profile(id: TargetId) -> TargetProfile {
    profiles::fetch_profile(id)
}

/// returns a list of all supported target profiles for the frontend ui
pub fn get_all_supported_profiles() -> Vec<TargetProfile> {
    profiles::fetch_all()
}
