// src-tauri/src/stealth/cpuid.rs
//
// CPUID hypervisor masking for Hyper-V VMs.
//
// This module removes all CPUID-level evidence that the guest is running
// under virtualization. Lockdown browsers and anti-cheat systems commonly
// check the following:
//
//   - CPUID leaf 0x40000000: Hypervisor signature string ("Microsoft Hv",
//     "KVMKVMKVM", "VMwareVMware", etc.) and max hypervisor leaf count
//   - CPUID leaf 0x1, ECX bit 31: Hypervisor present bit
//   - CPUID leaf 0x40000001: Hypervisor interface version
//   - MSR 0x40000000 range: Hypervisor-specific MSRs
//
// Hyper-V provides a first-class mechanism for this via
// `Set-VMProcessor -HideHypervisorPresent $true`, which handles most of
// the above automatically. For defense in depth, we also apply WMI-level
// CPUID mask overrides and verify the result.

use crate::stealth::StealthError;
use crate::vm::lifecycle::VmHandle;

use std::io::{Read, Write};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// CPUID leaf that contains the hypervisor vendor signature.
/// When a hypervisor is present, EAX holds the max hypervisor leaf,
/// and EBX/ECX/EDX hold a 12-character vendor ID string.
pub const CPUID_LEAF_HYPERVISOR_SIGNATURE: u32 = 0x4000_0000;

/// CPUID leaf 0x40000001 — hypervisor interface info.
pub const CPUID_LEAF_HYPERVISOR_INTERFACE: u32 = 0x4000_0001;

/// Base of the hypervisor-defined MSR range.
pub const MSR_HYPERVISOR_BASE: u32 = 0x4000_0000;

/// Default vendor string we want the VM to report (Intel bare metal).
/// This is what CPUID leaf 0 returns on a physical Intel machine.
#[allow(dead_code)]
const INTEL_VENDOR_STRING: &str = "GenuineIntel";

/// Default timeout for a PowerShell host invocation.
const HOST_PS_TIMEOUT: Duration = Duration::from_secs(90);

/// Poll granularity for the blocking runner's wait loop.
const SYNC_POLL: Duration = Duration::from_millis(50);

/// Deadline for the post-unmask verification loop.
const UNMASK_VERIFY_TIMEOUT_SECS: u64 = 120;

/// Shared argument vector for every host PowerShell invocation.
const PS_ARGS: [&str; 6] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
    "-",
];

// ---------------------------------------------------------------------------
// Public async API
// ---------------------------------------------------------------------------

/// Apply hypervisor masking to the target VM.
///
/// Combines the Hyper-V first-class `HideHypervisorPresent` toggle with a
/// WMI defense-in-depth pass that scrubs `Msvm_ProcessorSettingData`.
///
/// Safe to invoke while the VM is running (live update) or offline.
pub async fn mask_hypervisor(vm_handle: VmHandle) -> Result<(), StealthError> {
    let vm_name = &vm_handle.name;
    let script = build_mask_script(vm_name);

    let output = run_powershell_on_host(&script).await?;

    // Failure sentinels are emitted before the script exits, so check them
    // first — otherwise a hard failure reads as a missing sentinel.
    if output.contains("MASK_FAILED:HideHypervisorPresent") {
        return Err(StealthError::CpuidFailure(format!(
            "mask_hypervisor: HideHypervisorPresent failed. Output:\n{}",
            output
        )));
    }

    if !output.contains("MASK_COMPLETE") {
        return Err(StealthError::CpuidFailure(format!(
            "mask_hypervisor: script did not reach MASK_COMPLETE sentinel. \
             Output:\n{}",
            output
        )));
    }

    Ok(())
}

/// Remove hypervisor masking and restore the guest's visibility of the
/// virtualization layer. Used during rollback or VM teardown.
///
/// This performs a hard power cycle so the guest re-enumerates CPUID and
/// ACPI tables from scratch, then verifies via `Win32_ComputerSystem` that
/// `HypervisorPresent` is `True` before returning success.
pub async fn unmask_hypervisor(vm_handle: VmHandle) -> Result<(), StealthError> {
    let vm_name = &vm_handle.name;
    let script = build_unmask_script(vm_name);

    let output = run_powershell_on_host(&script).await?;

    if output.contains("Unmask verification failed") {
        return Err(StealthError::CpuidFailure(format!(
            "unmask_hypervisor: guest did not re-report HypervisorPresent \
             within {}s. Output:\n{}",
            UNMASK_VERIFY_TIMEOUT_SECS, output
        )));
    }

    Ok(())
}

/// Capability check: can the current host apply CPUID masking?
///
/// Requires the Hyper-V PowerShell module and a `Set-VMProcessor` that
/// exposes the `HideHypervisorPresent` parameter (Windows 10 1803+ /
/// Windows Server 2019+).
pub fn host_supports_masking() -> bool {
    let probe = r#"
$ErrorActionPreference = 'Stop'
try {
    $cmd = Get-Command Set-VMProcessor -ErrorAction Stop
    if ($cmd.Parameters.ContainsKey('HideHypervisorPresent')) {
        Write-Output 'SUPPORTED'
    } else {
        Write-Output 'UNSUPPORTED'
    }
} catch {
    Write-Output 'UNSUPPORTED'
}
"#;

    match std::process::Command::new("powershell.exe")
        .args(PS_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains("SUPPORTED"),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// PowerShell script templates
// ---------------------------------------------------------------------------

/// Primary masking script — sets HideHypervisorPresent and clears
/// virtualization extension exposure. Can apply while the VM is running
/// (live update) or offline.
pub fn build_mask_script(vm_name: &str) -> String {
    format!(
        r#"
$ErrorActionPreference = 'Stop'

$vmName = '{vm_name}'

# --- Hide hypervisor from guest ---
try {{
    Set-VMProcessor -VMName $vmName -HideHypervisorPresent $true
    Write-Output "MASK_APPLIED:HideHypervisorPresent"
}} catch {{
    Write-Output "MASK_FAILED:HideHypervisorPresent:$($_.Exception.Message)"
    exit 1
}}

# --- Do not expose virtualization extensions to guest ---
try {{
    Set-VMProcessor -VMName $vmName -ExposeVirtualizationExtensions $false
    Write-Output "MASK_APPLIED:ExposeVirtualizationExtensions"
}} catch {{
    # Not all CPU architectures support this; non-fatal
    Write-Output "MASK_SKIPPED:ExposeVirtualizationExtensions"
}}

# --- Disable resource metering so RDTSC/temperature counters look native ---
try {{
    Set-VMMemory -VMName $vmName -ResourceMeteringEnabled $false
    Write-Output "MASK_APPLIED:ResourceMetering"
}} catch {{
    Write-Output "MASK_SKIPPED:ResourceMetering:$($_.Exception.Message)"
}}

# --- Clear hypervisor present bit in CPUID leaf 0x1 via WMI ---
try {{
    $vm = Get-CimInstance -Namespace 'root\virtualization\v2' `
        -ClassName 'Msvm_ComputerSystem' `
        -Filter "ElementName = '$vmName'"

    if ($null -ne $vm) {{
        $procSD = $vm | Get-CimAssociatedInstance `
            -ResultClassName 'Msvm_ProcessorSettingData'

        if ($null -ne $procSD) {{
            # Strip hypervisor hints from the processor setting data
            $procSD.HideHypervisorPresent = $true
            $procSD.ExposeVirtualizationExtensions = $false

            $vmms = Get-CimInstance -Namespace 'root\virtualization\v2' `
                -ClassName 'Msvm_VirtualSystemManagementService'

            if ($null -ne $vmms) {{
                $cimSerializer = [Microsoft.Management.Infrastructure.Serialization.CimSerializer]::Create()
                $sdBytes = $cimSerializer.Serialize($procSD, [Microsoft.Management.Infrastructure.Serialization.InstanceSerializationOptions]::None)
                $sdText = [System.Text.Encoding]::Unicode.GetString($sdBytes)
                $result = Invoke-CimMethod -InputObject $vmms `
                    -MethodName 'ModifyResourceSettings' `
                    -Arguments @{{ ResourceSettings = @($sdText) }}
                if ($result.ReturnValue -eq 0 -or $result.ReturnValue -eq 4096) {{
                    Write-Output "MASK_APPLIED:WmiProcessorSettings"
                }} else {{
                    Write-Output "MASK_SKIPPED:WmiProcessorSettings:ReturnValue=$($result.ReturnValue)"
                }}
            }}
        }}
    }}
}} catch {{
    # WMI path is defense-in-depth; primary path already succeeded
    Write-Output "MASK_SKIPPED:WmiProcessorSettings:$($_.Exception.Message)"
}}

Write-Output "MASK_COMPLETE"
"#,
        vm_name = vm_name
    )
}

/// Unmask script — restores hypervisor visibility. Used during rollback
/// or when destroying the VM.
pub fn build_unmask_script(vm_name: &str) -> String {
    let mut s = String::new();

    s.push_str("$ErrorActionPreference = 'Stop'\n\n");
    s.push_str("# Unmask script — restoring hypervisor presence\n");
    s.push_str("# Target VM: ");
    s.push_str(vm_name);
    s.push_str("\n\n");

    // 1. Re-enable hypervisor CPUID leaf reporting (clear HideHypervisorPresent)
    s.push_str("# Restore CPUID leaf 0x40000000+ disclosures\n");
    s.push_str(&format!(
        "try {{ Set-VMProcessor -VMName \"{}\" -HideHypervisorPresent $false }} \
         catch {{ Write-Output \"UNMASK_SKIPPED:HideHypervisorPresent:$($_.Exception.Message)\" }}\n",
        vm_name
    ));
    s.push_str(&format!(
        "try {{ Set-VMProcessor -VMName \"{}\" -ExposeVirtualizationExtensions $true }} \
         catch {{ Write-Output \"UNMASK_SKIPPED:ExposeVirtualizationExtensions:$($_.Exception.Message)\" }}\n\n",
        vm_name
    ));

    // 2. Restore RDTSC/temperature counters by re-enabling resource metering
    s.push_str("# Restore RDTSC/temperature counters\n");
    s.push_str(&format!(
        "try {{ Set-VMMemory -VMName \"{}\" -ResourceMeteringEnabled $true }} \
         catch {{ Write-Output \"UNMASK_SKIPPED:ResourceMetering:$($_.Exception.Message)\" }}\n\n",
        vm_name
    ));

    // 3. Unhook the disk pass-through shim so the real underlying
    //    storage controller becomes visible inside the guest again.
    s.push_str("# Remove disk pass-through shim\n");
    s.push_str(&format!(
        "try {{ Get-VMHardDiskDrive -VMName \"{}\" | ",
        vm_name
    ));
    s.push_str("Where-Object { $_.Path -like '*shim*.vhdx' } | ");
    s.push_str("Remove-VMHardDiskDrive } catch { Write-Output \"UNMASK_SKIPPED:ShimRemoval:$($_.Exception.Message)\" }\n\n");

    // 4. Reattach the original disk image (if the canonical path is present)
    s.push_str("# Reattach original disk image if canonical path exists\n");
    s.push_str(&format!(
        "$origDisk = Join-Path $env:VM_DISKS \"{}.vhdx\"\n",
        vm_name
    ));
    s.push_str("if (Test-Path $origDisk) {\n");
    s.push_str(&format!(
        "    $already = Get-VMHardDiskDrive -VMName \"{}\" | Where-Object {{ $_.Path -eq $origDisk }}\n",
        vm_name
    ));
    s.push_str("    if (-not $already) {\n");
    s.push_str(&format!(
        "        try {{ Add-VMHardDiskDrive -VMName \"{}\" -Path $origDisk }} \
         catch {{ Write-Output \"UNMASK_SKIPPED:AddOrigDisk:$($_.Exception.Message)\" }}\n",
        vm_name
    ));
    s.push_str("    }\n");
    s.push_str("}\n\n");

    // 5. Signal the guest-side agent to undo registry edits.
    s.push_str("# Signal guest-side agent to undo registry edits\n");
    s.push_str(&format!(
        "try {{\n    $maskKey = \"HKLM:\\SOFTWARE\\MaskAgent\\{}\"\n",
        vm_name
    ));
    s.push_str("    if (-not (Test-Path $maskKey)) { New-Item -Path $maskKey -Force | Out-Null }\n");
    s.push_str("    Set-ItemProperty -Path $maskKey -Name \"State\" -Value \"Unmasking\" -Type String\n");
    s.push_str("} catch { Write-Output \"UNMASK_SKIPPED:MaskAgentState:$($_.Exception.Message)\" }\n\n");

    // 6. Full power cycle so the guest actually re-enumerates
    //    CPUID and ACPI tables from scratch.
    s.push_str("# Hard reset required for CPUID re-enumeration\n");
    s.push_str(&format!(
        "try {{ Stop-VM -Name \"{}\" -TurnOff -Force -Confirm:$false }} \
         catch {{ Write-Output \"UNMASK_SKIPPED:StopVM:$($_.Exception.Message)\" }}\n",
        vm_name
    ));
    s.push_str("Start-Sleep -Seconds 3\n");
    s.push_str(&format!(
        "try {{ Start-VM -Name \"{}\" }} catch {{ Write-Error \"Unmask failed: cannot start VM: $($_.Exception.Message)\" }}\n\n",
        vm_name
    ));

    // 7. Verification pass — confirm the hypervisor bit is back.
    s.push_str(&format!(
        "# Verify unmask succeeded (timeout {}s)\n",
        UNMASK_VERIFY_TIMEOUT_SECS
    ));
    s.push_str(&format!(
        "$deadline = (Get-Date).AddSeconds({})\n",
        UNMASK_VERIFY_TIMEOUT_SECS
    ));
    s.push_str("$ok = $false\n");
    s.push_str("while ((Get-Date) -lt $deadline) {\n");
    s.push_str(&format!("    $state = (Get-VM -Name \"{}\").State\n", vm_name));
    s.push_str("    if ($state -eq 'Running') {\n");
    s.push_str("        try {\n");
    s.push_str(&format!(
        "            $probe = Invoke-Command -VMName \"{}\" ",
        vm_name
    ));
    s.push_str("-ScriptBlock { (Get-CimInstance Win32_ComputerSystem).HypervisorPresent } -ErrorAction Stop\n");
    s.push_str("            if ($probe) { $ok = $true; break }\n");
    s.push_str("        } catch {\n");
    s.push_str("            # Guest may not have WinRM ready yet; retry\n");
    s.push_str("        }\n");
    s.push_str("    }\n");
    s.push_str("    Start-Sleep -Seconds 5\n");
    s.push_str("}\n\n");
    s.push_str("if (-not $ok) {\n");
    s.push_str("    Write-Error 'Unmask verification failed'\n");
    s.push_str("} else {\n");
    s.push_str("    Write-Output 'UNMASK_COMPLETE'\n");
    s.push_str("}\n");

    s
}

// ---------------------------------------------------------------------------
// Host PowerShell invocation
// ---------------------------------------------------------------------------

/// Merge stdout and stderr into one blob so callers can look for sentinels
/// regardless of which stream PowerShell chose.
fn combine_streams(stdout: String, stderr: String) -> String {
    if stderr.is_empty() {
        stdout
    } else {
        format!("{}\n[STDERR]\n{}", stdout, stderr)
    }
}

/// Run a PowerShell script on the host and capture combined stdout/stderr.
///
/// The script is passed via stdin (`-Command -`) so we never hit command-line
/// length limits and quoting rules stay simple. A hard timeout guards against
/// hung WinRM/VSMS calls.
pub(crate) async fn run_powershell_on_host(script: &str) -> Result<String, StealthError> {
    let mut child = Command::new("powershell.exe")
        .args(PS_ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            StealthError::HostInvocation(format!("failed to spawn powershell.exe: {}", e))
        })?;

    // Feed the script over stdin.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes()).await.map_err(|e| {
            StealthError::HostInvocation(format!("failed to write script to stdin: {}", e))
        })?;
        // Dropping stdin closes the pipe, signalling EOF to PowerShell.
        drop(stdin);
    }

    let output = match timeout(HOST_PS_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Err(StealthError::HostInvocation(format!(
                "powershell.exe wait failed: {}",
                e
            )))
        }
        Err(_) => {
            return Err(StealthError::PowerShellFailure {
                exit_code: -1,
                stderr: format!("powershell.exe timed out after {:?}", HOST_PS_TIMEOUT),
            })
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(combine_streams(stdout, stderr))
}

/// Blocking variant of [`run_powershell_on_host`], for call sites that are
/// not on a Tokio runtime or cannot await (rollback paths).
///
/// Stdout/stderr are drained on helper threads so a large script result can
/// never deadlock against the wait loop.
pub fn run_powershell_on_host_sync(script: &str) -> Result<String, StealthError> {
    let mut child = std::process::Command::new("powershell.exe")
        .args(PS_ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            StealthError::HostInvocation(format!("failed to spawn powershell.exe: {}", e))
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes()).map_err(|e| {
            StealthError::HostInvocation(format!("failed to write script to stdin: {}", e))
        })?;
    }

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();

    let out_reader = out_pipe.take().map(|mut p| {
        thread::spawn(move || {
            let mut buf = String::new();
            let _ = p.read_to_string(&mut buf);
            buf
        })
    });
    let err_reader = err_pipe.take().map(|mut p| {
        thread::spawn(move || {
            let mut buf = String::new();
            let _ = p.read_to_string(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + HOST_PS_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(StealthError::PowerShellFailure {
                    exit_code: -1,
                    stderr: format!("powershell.exe timed out after {:?}", HOST_PS_TIMEOUT),
                });
            }
            Ok(None) => thread::sleep(SYNC_POLL),
            Err(e) => {
                let _ = child.kill();
                return Err(StealthError::HostInvocation(format!(
                    "powershell.exe wait failed: {}",
                    e
                )));
            }
        }
    }

    let stdout = out_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = err_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    Ok(combine_streams(stdout, stderr))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mask_unmask_cycle() {
        // Assumes a Hyper-V host with a test VM named "TestVM".
        let vm_handle = VmHandle::new("TestVM");

        let mask_result = mask_hypervisor(vm_handle.clone()).await;
        assert!(mask_result.is_ok(), "Masking failed: {:?}", mask_result);

        let unmask_result = unmask_hypervisor(vm_handle).await;
        assert!(unmask_result.is_ok(), "Unmasking failed: {:?}", unmask_result);
    }

    #[test]
    fn host_reports_masking_support() {
        assert!(host_supports_masking(), "Host does not support masking");
    }

    #[test]
    fn mask_script_sets_hide_hypervisor() {
        let script = build_mask_script("TestVM");
        assert!(script.contains("Set-VMProcessor -VMName $vmName -HideHypervisorPresent $true"));
        assert!(script.contains("MASK_COMPLETE"));
    }

    #[test]
    fn unmask_script_clears_hide_hypervisor() {
        let script = build_unmask_script("TestVM");
        assert!(script.contains("Set-VMProcessor -VMName \"TestVM\" -HideHypervisorPresent $false"));
        assert!(script.contains("UNMASK_COMPLETE"));
    }

    #[test]
    fn scripts_embed_the_vm_name() {
        assert!(build_mask_script("Alpha").contains("'Alpha'"));
        assert!(build_unmask_script("Beta").contains("\"Beta\""));
    }

    #[test]
    fn mask_script_has_balanced_format_braces() {
        // A stray single brace would make format! panic; this asserts the
        // rendered script still contains real PowerShell blocks.
        let script = build_mask_script("TestVM");
        assert!(script.contains("try {"));
        assert!(script.contains("catch {"));
    }
}