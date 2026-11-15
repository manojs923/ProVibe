// src-tauri/src/patches/loader.rs

use crate::hyper_v::{build_guest_invocation, run_host_ps, GuestCredentials};
use crate::patches::{Patch, PatchType};
use crate::vm::config::VmConfig;

pub async fn apply_patches(patches: Vec<Patch>, vm_config: &VmConfig) -> Result<(), String> {
    for patch in patches {
        match patch.patch_type {
            // Guest-side: transported through PowerShell Direct with real
            // credentials, so `Invoke-Command` can actually authenticate.
            PatchType::RegistryMod
            | PatchType::ServiceDisable
            | PatchType::WmiHide
            | PatchType::EtwSuppress => {
                let invocation = build_guest_invocation(
                    &vm_config.vm_name,
                    &GuestCredentials::default(),
                    &patch.payload,
                );
                run_host_ps(&invocation)
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to apply patch '{}' ({}): {}",
                            patch.id, patch.name, e
                        )
                    })?;
            }

            // Guest-side driver registration. Payload is the staged .sys path.
            PatchType::DriverInject => {
                let driver_name = format!("{}_svc", patch.id);
                let guest_script = format!(
                    r#"$ErrorActionPreference = 'Stop'
sc.exe create {svc} type= kernel binPath= '{path}'
if ($LASTEXITCODE -ne 0) {{ throw "sc create failed: $LASTEXITCODE" }}
sc.exe start {svc}
if ($LASTEXITCODE -ne 0) {{ throw "sc start failed: $LASTEXITCODE" }}
"#,
                    svc = driver_name,
                    path = patch.payload.replace('\'', "''")
                );

                let invocation = build_guest_invocation(
                    &vm_config.vm_name,
                    &GuestCredentials::default(),
                    &guest_script,
                );
                run_host_ps(&invocation).await.map_err(|e| {
                    format!(
                        "failed to inject driver '{}' ({}): {}",
                        patch.id, patch.name, e
                    )
                })?;
            }

            // Host-side: SMBIOS is a VM property, not a guest one.
            // Payload is a Hyper-V cmdlet fragment taking `-VMName`.
            PatchType::SmbiosSpoof => {
                let host_script = format!(
                    "$ErrorActionPreference = 'Stop'\n{} -VMName '{}'",
                    patch.payload,
                    vm_config.vm_name.replace('\'', "''")
                );
                run_host_ps(&host_script).await.map_err(|e| {
                    format!(
                        "failed to spoof smbios '{}' ({}): {}",
                        patch.id, patch.name, e
                    )
                })?;
            }
        }

        println!("successfully applied patch: {}", patch.name);
    }

    Ok(())
}