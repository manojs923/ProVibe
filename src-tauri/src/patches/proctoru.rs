// src-tauri/src/patches/proctoru.rs

use super::{Patch, PatchType};

pub fn get_patches() -> Vec<Patch> {
    vec![
        Patch {
            id: "pu_ntquery_hook".to_string(),
            name: "ntquerysysteminformation castration".to_string(),
            description: "blinds proctoruservice.exe from seeing our stealth processes by hooking the syscall".to_string(),
            patch_type: PatchType::DriverInject,
            payload: "c:\\stealth\\ntquery_hook.sys".to_string(),
        },
        Patch {
            id: "pu_winsta_hide".to_string(),
            name: "winstation rdp scrub".to_string(),
            description: "hooks window station enumeration to hide rdp-tcp artifacts from their dirty little eyes".to_string(),
            patch_type: PatchType::DriverInject,
            payload: "c:\\stealth\\winsta_hook.sys".to_string(),
        },
        Patch {
            id: "pu_env_nuke".to_string(),
            name: "rdp environment variable purge".to_string(),
            description: "strips sessionname and clientname env vars that scream 'i am using rdp'".to_string(),
            patch_type: PatchType::RegistryMod,
            payload: "remove-itemproperty -path 'hklm:\\system\\currentcontrolset\\control\\session manager\\environment' -name 'clientname' -erroraction silentlycontinue; remove-itemproperty -path 'hklm:\\system\\currentcontrolset\\control\\session manager\\environment' -name 'sessionname' -erroraction silentlycontinue".to_string(),
        }
    ]
}