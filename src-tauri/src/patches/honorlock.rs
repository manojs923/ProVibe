// src-tauri/src/patches/honorlock.rs

use super::{Patch, PatchType};

pub fn get_patches() -> Vec<Patch> {
    vec![
        Patch {
            id: "hl_dxgi_castration".to_string(),
            name: "dxgi duplicateoutput hook".to_string(),
            description: "hooks dxgi to return dxgi_error_not_currently_available. forces their spyware to use software capture so it can't see the host overlay.".to_string(),
            patch_type: PatchType::DriverInject,
            payload: "c:\\stealth\\dxgi_hook.sys".to_string(),
        },
        Patch {
            id: "hl_webrtc_plug".to_string(),
            name: "webrtc subnet masking".to_string(),
            description: "stops their invasive chrome extension from probing local subnets to find the hyper-v switch.".to_string(),
            patch_type: PatchType::RegistryMod,
            payload: "if (!(test-path 'hklm:\\software\\policies\\google\\chrome')) { new-item -path 'hklm:\\software\\policies\\google\\chrome' -force | out-null }; set-itemproperty -path 'hklm:\\software\\policies\\google\\chrome' -name 'webrtciphandlingpolicy' -value 'default_public_interface_only' -type string -force".to_string(),
        }
    ]
}