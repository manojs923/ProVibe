// src-tauri/src/patches/loader.rs

use super::{Patch, PatchType};
use std::process::Command;

/// injects patches into the vm via powershell remoting over the loopback tunnel
/// or applies host-side hyper-v configurations depending on the patch type.
pub fn apply_patches(patches: Vec<Patch>, vm_id: &str) -> Result<(), String> {
    for patch in patches {
        let ps_cmd = match patch.patch_type {
            PatchType::RegistryMod | PatchType::ServiceDisable | PatchType::WmiHide | PatchType::EtwSuppress => {
                // direct powershell execution inside the guest vm
                format!(
                    "Invoke-Command -VMName {} -ScriptBlock {{ {} }}",
                    vm_id, patch.payload
                )
            },
            PatchType::DriverInject => {
                // uses sc.exe inside the guest to register and start a kernel driver.
                // assumes payload is the absolute path to the staged .sys file (e.g., C:\stealth\hook.sys)
                let driver_name = format!("{}_svc", patch.id);
                format!(
                    "Invoke-Command -VMName {} -ScriptBlock {{ sc.exe create {} type= kernel binPath= '{}'; sc.exe start {} }}",
                    vm_id, driver_name, patch.payload, driver_name
                )
            },
            PatchType::SmbiosSpoof => {
                // smbios and hypervisor bit masking must be executed on the HOST, targeting the vm.
                // assumes payload is a hyper-v cmdlet like `Set-VMBios -SerialNumber ...`
                format!("{} -VMName {}", patch.payload, vm_id)
            }
        };

        let output = Command::new("powershell")
            .args(&["-NoProfile", "-NonInteractive", "-Command", &ps_cmd])
            .output()
            .map_err(|e| format!("failed to spawn powershell: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("failed to apply patch '{}' ({}): {}", patch.id, patch.name, stderr.trim()));
        }
        
        println!("successfully applied patch: {}", patch.name);
    }
    
    Ok(())
}