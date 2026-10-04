// src-tauri/src/patches/proctorio.rs

use super::{Patch, PatchType};

pub fn get_patches() -> Vec<Patch> {
    vec![
        Patch {
            id: "proc_chrome_integrity".to_string(),
            name: "chrome extension integrity bypass".to_string(),
            description: "fucks with chrome policies to bypass proctorio extension integrity checks".to_string(),
            patch_type: PatchType::RegistryMod,
            payload: "if (!(test-path 'hklm:\\software\\policies\\google\\chrome')) { new-item -path 'hklm:\\software\\policies\\google\\chrome' -force | out-null }; set-itemproperty -path 'hklm:\\software\\policies\\google\\chrome' -name 'extensioninstallallowlist' -value '1' -type string -force".to_string(),
        },
        Patch {
            id: "proc_webrtc_leak".to_string(),
            name: "webrtc ip leak suppression".to_string(),
            description: "forces chrome to route webrtc through public interfaces only, hiding the internal vm subnet from their javascript probes".to_string(),
            patch_type: PatchType::RegistryMod,
            payload: "set-itemproperty -path 'hklm:\\software\\policies\\google\\chrome' -name 'webrtciphandlingpolicy' -value 'default_public_interface_only' -type string -force".to_string(),
        },
        Patch {
            id: "proc_mac_spoof".to_string(),
            name: "getadaptersinfo mac spoof".to_string(),
            description: "ring-0 hook to spoof physical adapter mac addresses so they don't see hyper-v ouis".to_string(),
            patch_type: PatchType::DriverInject,
            payload: "c:\\stealth\\mac_spoof.sys".to_string(),
        }
    ]
}