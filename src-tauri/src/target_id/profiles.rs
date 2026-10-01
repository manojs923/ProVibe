use super::{DetectionVector, TargetId, TargetProfile, VMConfigOverride};

/// returns the full static database of known fedware
pub fn fetch_all() -> Vec<TargetProfile> {
    vec![
        TargetProfile {
            target_id: TargetId::Respondus,
            display_name: "respondus lockdown browser".to_string(),
            detection_vectors: DetectionVector {
                process_names: vec![
                    "LockDownBrowser.exe".to_string(),
                    "Respondus LockDown Browser.exe".to_string(),
                ],
                registry_keys: vec![
                    r#"HKLM\SOFTWARE\Respondus"#.to_string(),
                    r#"HKCU\Software\Respondus"#.to_string(),
                ],
                service_names: vec!["RespondusLLB".to_string()],
                window_classes: vec!["RespondusLDB_Window".to_string()],
            },
            patch_set_id: "respondus_core".to_string(),
            vm_config_override: Some(VMConfigOverride {
                requires_internet: true,
                min_ram_gb: 4,
                min_cores: 2,
                strict_timing_spoof: true, // respondus checks rdtsc timing deltas
            }),
        },
        TargetProfile {
            target_id: TargetId::Proctorio,
            display_name: "proctorio".to_string(),
            detection_vectors: DetectionVector {
                // proctorio is a chrome extension, but we look for its native messaging host or specific browser flags
                process_names: vec!["chrome.exe".to_string(), "msedge.exe".to_string()],
                registry_keys: vec![
                    r#"HKLM\SOFTWARE\Google\Chrome\Extensions\fpjooibignfceippgfhcgmaaohldcgno"#.to_string(), // common extension id
                ],
                service_names: vec![],
                window_classes: vec![],
            },
            patch_set_id: "proctorio_webrtc_block".to_string(),
            vm_config_override: Some(VMConfigOverride {
                requires_internet: true,
                min_ram_gb: 6, // chrome eats ram, don't let the vm swap
                min_cores: 4,
                strict_timing_spoof: false,
            }),
        },
        TargetProfile {
            target_id: TargetId::ProctorU,
            display_name: "proctoru".to_string(),
            detection_vectors: DetectionVector {
                process_names: vec![
                    "ProctorUService.exe".to_string(),
                    "LogMeInRescue.exe".to_string(),
                    "Support-LogMeInRescue.exe".to_string(),
                ],
                registry_keys: vec![
                    r#"HKLM\SOFTWARE\LogMeInRescue"#.to_string(),
                    r#"HKLM\SYSTEM\CurrentControlSet\Services\ProctorUService"#.to_string(),
                ],
                service_names: vec!["ProctorUService".to_string(), "LMIRescue".to_string()],
                window_classes: vec!["LMIRescueWindowClass".to_string()],
            },
            patch_set_id: "proctoru_stealth".to_string(),
            vm_config_override: Some(VMConfigOverride {
                requires_internet: true,
                min_ram_gb: 8, // live janny poking around, needs to feel like a real rig
                min_cores: 4,
                strict_timing_spoof: true,
            }),
        },
        TargetProfile {
            target_id: TargetId::Honorlock,
            display_name: "honorlock".to_string(),
            detection_vectors: DetectionVector {
                process_names: vec!["chrome.exe".to_string()],
                registry_keys: vec![
                    r#"HKLM\SOFTWARE\Google\Chrome\Extensions\hnbpdiengicdefcjecjbnjnoifekhgfc"#.to_string(),
                ],
                service_names: vec![],
                window_classes: vec![],
            },
            patch_set_id: "honorlock_dxgi_hook".to_string(),
            vm_config_override: Some(VMConfigOverride {
                requires_internet: true,
                min_ram_gb: 4,
                min_cores: 2,
                strict_timing_spoof: false,
            }),
        },
        TargetProfile {
            target_id: TargetId::Examity,
            display_name: "examity".to_string(),
            detection_vectors: DetectionVector {
                process_names: vec![
                    "ExamityBrowser.exe".to_string(),
                    "zoom.exe".to_string(), // examity heavily relies on zoom screen share
                ],
                registry_keys: vec![r#"HKCU\Software\Examity"#.to_string()],
                service_names: vec![],
                window_classes: vec!["ZPContentViewWndClass".to_string()], // zoom window class
            },
            patch_set_id: "examity_smbios_spoof".to_string(),
            vm_config_override: Some(VMConfigOverride {
                requires_internet: true,
                min_ram_gb: 8,
                min_cores: 4,
                strict_timing_spoof: false,
            }),
        },
    ]
}

/// fetches a specific profile by its enum variant.
/// falls back to generic if something goes wrong.
pub fn fetch_profile(id: TargetId) -> TargetProfile {
    fetch_all()
        .into_iter()
        .find(|p| p.target_id == id)
        .unwrap_or_else(|| get_generic_profile())
}

/// fallback profile for unidentified glowie tech
fn get_generic_profile() -> TargetProfile {
    TargetProfile {
        target_id: TargetId::Generic,
        display_name: "generic/unknown".to_string(),
        detection_vectors: DetectionVector {
            process_names: vec![],
            registry_keys: vec![],
            service_names: vec![],
            window_classes: vec![],
        },
        patch_set_id: "generic_broad_spectrum".to_string(),
        vm_config_override: Some(VMConfigOverride {
            requires_internet: true,
            min_ram_gb: 4,
            min_cores: 2,
            strict_timing_spoof: true,
        }),
    }
}