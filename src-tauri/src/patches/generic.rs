// src-tauri/src/patches/generic.rs

use super::{Patch, PatchType};

pub fn get_patches() -> Vec<Patch> {
    vec![
        Patch {
            id: "gen_hypervisor_mask".to_string(),
            name: "cpuid hypervisor bit mask".to_string(),
            description: "generic masking of the hypervisor present bit via hyper-v config".to_string(),
            patch_type: PatchType::SmbiosSpoof, // executed on host
            payload: "set-vmprocessor -exposevirtualizationextensions $false".to_string(),
        },
        Patch {
            id: "gen_smbios_spoof".to_string(),
            name: "oem smbios spoof".to_string(),
            description: "makes the vm look like a generic dell optiplex".to_string(),
            patch_type: PatchType::SmbiosSpoof, // executed on host
            payload: "set-vmbios -serialnumber 'cn123456789' -manufacturer 'dell inc.' -systemfamily 'optiplex'".to_string(),
        },
        Patch {
            id: "gen_integration_scrub".to_string(),
            name: "hyper-v integration scrub".to_string(),
            description: "nukes vm guest additions and virtual machine registry artifacts".to_string(),
            patch_type: PatchType::RegistryMod,
            payload: "remove-item -path 'hklm:\\software\\microsoft\\virtual machine' -recurse -force -erroraction silentlycontinue; get-service -name vmic* | stop-service -force -erroraction silentlycontinue; get-service -name vmic* | set-service -startuptype disabled -erroraction silentlycontinue".to_string(),
        },
        Patch {
            id: "gen_process_hide".to_string(),
            name: "generic process hiding".to_string(),
            description: "dkom hook to unlink specific pids from the eprocess list".to_string(),
            patch_type: PatchType::DriverInject,
            payload: "c:\\stealth\\hide_proc.sys".to_string(),
        },
        Patch {
            id: "gen_timing_norm".to_string(),
            name: "generic timing normalization".to_string(),
            description: "rdtsc vmexit timing normalization to defeat cpuid vmexit delta checks".to_string(),
            patch_type: PatchType::DriverInject,
            payload: "c:\\stealth\\tsc_hook.sys".to_string(),
        }
    ]
}