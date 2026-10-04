// src-tauri/src/patches/respondus.rs

use super::{Patch, PatchType};

pub fn get_patches() -> Vec<Patch> {
    vec![
        Patch {
            id: "resp_watchdog_kill".to_string(),
            name: "disable respondusllb service".to_string(),
            description: "murders the local watchdog service before it phones home".to_string(),
            patch_type: PatchType::ServiceDisable,
            payload: "Stop-Service -Name RespondusLLB -Force -ErrorAction SilentlyContinue; Set-Service -Name RespondusLLB -StartupType Disabled -ErrorAction SilentlyContinue".to_string(),
        },
        Patch {
            id: "resp_reg_bypass".to_string(),
            name: "respondus registry bypass".to_string(),
            description: "forces the allowvm flag and zeroes out vmdetection in their own config".to_string(),
            patch_type: PatchType::RegistryMod,
            payload: "if (!(Test-Path 'HKLM:\\SOFTWARE\\Respondus')) { New-Item -Path 'HKLM:\\SOFTWARE\\Respondus' -Force | Out-Null }; Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Respondus' -Name 'VMDetection' -Value 0 -Type DWord -Force; Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Respondus' -Name 'AllowVM' -Value 1 -Type DWord -Force".to_string(),
        },
        Patch {
            id: "resp_device_scrub".to_string(),
            name: "virtual device id scrub".to_string(),
            description: "finds and renames vm-specific hardware strings (hyper-v, virtual) in the device enumeration registry".to_string(),
            patch_type: PatchType::RegistryMod,
            payload: "$keys = Get-ChildItem -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Enum' -Recurse -ErrorAction SilentlyContinue | Where-Object { $_.GetValue('FriendlyName') -match 'Hyper-V|Virtual|VMware|VBox' }; foreach ($key in $keys) { $newName = $key.GetValue('FriendlyName') -replace 'Hyper-V|Virtual|VMware|VBox', 'Generic'; Set-ItemProperty -Path $key.PSPath -Name 'FriendlyName' -Value $newName -Force }".to_string(),
        },
        Patch {
            id: "resp_qpf_patch".to_string(),
            name: "timing normalization".to_string(),
            description: "patches queryperformancefrequency to match bare metal timing via ring-0 hook".to_string(),
            patch_type: PatchType::DriverInject,
            payload: "C:\\stealth\\qpf_hook.sys".to_string(),
        }
    ]
}