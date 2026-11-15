use std::fmt;

use serde::{Deserialize, Serialize};
use sysinfo::System;
use tokio::task;

use self::matcher::match_artifacts;
use self::scanner::scan_host_artifacts;
pub use self::profiles::{fetch_all, fetch_profile};

pub mod matcher;
pub mod profiles;
pub mod scanner;

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
                    // argv[0] — exe path, not a real window title. Good enough
                    // to fingerprint which binary it is; swap for EnumWindows
                    // if the UI ever needs to display a title.
                    window_title: process.cmd().first().map(|s| s.clone()),
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

/// Kick off a one-shot host scan and profile resolution off the caller's
/// thread. Returns immediately; the work happens on a background task.
pub async fn start_detection_loop(_app_handle: &tauri::AppHandle) {
    let artifacts = scan_host_artifacts();
    let profiles = fetch_all();

    task::spawn(async move {
        if artifacts.is_empty() {
            return;
        }

        // One host scan, matched once, then each candidate profile resolved.
        if let Some(matched) = match_artifacts(artifacts) {
            println!("Matched artifact: {:?}", matched);
            let _ = fetch_profile(matched.target_id);
        }

        // Profiles drive the override set; nothing to do with them directly
        // yet beyond enumerating them for the caller's logging.
        for profile in &profiles {
            println!("considered profile: {}", profile.display_name);
        }
    });
}

/// A specific lockdown browser or proctoring system.
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

/// The kind of detection vector.
/// Used by the stealth module to decide what evasion measures are needed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DetectionVectorKind {
    ProcessPresent,
    RegistryKey,
    ServiceRunning,
    WindowClass,
    TimingAnomaly,
}

/// Detection vectors used by the matcher to identify the target.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetectionVector {
    pub process_names: Vec<String>,
    pub registry_keys: Vec<String>,
    pub service_names: Vec<String>,
    pub window_classes: Vec<String>,
}

impl DetectionVector {
    /// Returns all detection vector kinds this vector covers.
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

    /// Checks if this vector contains a specific kind.
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

/// How the VM should be configured to evade this specific target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VMConfigOverride {
    pub requires_internet: bool,
    pub min_ram_gb: u32,
    pub min_cores: u32,
    pub strict_timing_spoof: bool,
}

/// Complete profile for a target: how to detect it, and how to patch the VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetProfile {
    pub target_id: TargetId,
    pub display_name: String,
    pub detection_vectors: DetectionVector,
    pub patch_set_id: String,
    pub vm_config_override: Option<VMConfigOverride>,
}

/// Attempt to identify a running proctoring system on the host OS.
///
/// Warning: if the architecture is purely VM-based, this only detects a
/// target the user accidentally launched on the bare metal host.
pub fn identify_target() -> Option<TargetProfile> {
    let artifacts = scanner::scan_host_artifacts();

    if artifacts.is_empty() {
        return None;
    }

    matcher::match_artifacts(artifacts)
}

/// Retrieve the static profile for a known target.
pub fn get_profile(id: TargetId) -> TargetProfile {
    profiles::fetch_profile(id)
}

/// All supported target profiles, for the frontend UI.
pub fn get_all_supported_profiles() -> Vec<TargetProfile> {
    profiles::fetch_all()
}