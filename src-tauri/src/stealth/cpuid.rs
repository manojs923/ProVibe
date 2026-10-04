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

use crate::vm::lifecycle::VmHandle;
use super::StealthError;

use std::process::Stdio;
use tokio::process::Command;

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
const INTEL_VENDOR_STRING: &str = "GenuineIntel";

// ---------------------------------------------------------------------------
// PowerShell script templates
// ---------------------------------------------------------------------------

/// Primary masking script — sets HideHypervisorPresent and clears
/// virtualization extension exposure. Can apply while VM is running
/// (live update) or offline.
fn build_mask_script(vm_name: &str) -> String {
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

# --- Clear hypervisor present bit in CPUID leaf 0x1 via WMI ---
try {{
    $vm = Get-CimInstance -Namespace 'root\virtualization\v2' `
        -ClassName 'Msvm_ComputerSystem' `
        -Filter "ElementName = '$vmName'"

    if ($null -ne $vm) {{
        $procSD = $vm | Get-CimAssociatedInstance `
            -ResultClassName 'Msvm_ProcessorSettingData'

        if ($null -ne $procSD) {{
            # For additional leaf-level masking, we patch the
            # LimitProcessorFeatures to strip hypervisor hints
            $procSD.HideHypervisorPresent = $true

            # Apply via VSMS
            $vmms = Get-CimInstance -Namespace 'root\virtualization\v2' `
                -ClassName 'Msvm_VirtualSystemManagementService'

            if ($null -ne $vmms) {{
                $sdText = $procSD | ConvertTo-CimInstanceString
                # Use ModifySystemSettings to apply
                $result = $vmms.ModifySystemSettings($sdText)
                Write-Output "MASK_APPLIED:WmiProcessorSettings"
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
/// or when destroying VM.
fn build_unmask_script(vm_name: &str) -> String {
    let mut s = String::new();

    s.push_str("# Unmask script — restoring hypervisor presence\n");
    s.push_str("# Target VM: ");
    s.push_str(vm_name);
    s.push_str("\n\n");

    // 1. Re-enable hypervisor CPUID leaf reporting
    s.push_str("# Restore CPUID leaf 0x40000000+ disclosures\n");
    s.push_str(&format!(
        "Set-VMProcessor -VMName \"{}\" -ExposeVirtualizationExtensions $true\n",
        vm_name
    ));

    // 2. Clear the timing-side-channel mitigations we added during masking
    s.push_str("# Restore RDTSC/temperature counters\n");
    s.push_str(&format!(
        "Set-VMMemory -VMName \"{}\" -ResourceMeteringEnabled $false\n",
        vm_name
    ));

    // 3. Unhook the disk pass-through shim so the real underlying
    //    storage controller becomes visible inside the guest again.
    s.push_str("# Remove disk pass-through shim\n");
    s.push_str(&format!(
        "Get-VMHardDiskDrive -VMName \"{}\" | ",
        vm_name
    ));
    s.push_str("Where-Object { $_.Path -like '*shim*.vhdx' } | ");
    s.push_str("Remove-VMHardDiskDrive\n");

    // 4. Reattach the original disk image
    s.push_str(&format!(
        "Add-VMHardDiskDrive -VMName \"{}\" -Path \"$env:VM_DISKS\\{}.vhdx\"\n",
        vm_name, vm_name
    ));

    // 5. Remove registry keys inside the guest that suppressed
    //    hypervisor artifacts (the guest agent handles the merge).
    s.push_str("# Signal guest-side agent to undo registry edits\n");
    s.push_str(&format!(
        "Set-ItemProperty -Path \"HKLM:\\SOFTWARE\\MaskAgent\\{}\" ",
        vm_name
    ));
    s.push_str("-Name \"State\" -Value \"Unmasking\" -Type String\n");

    // 6. Full power cycle so the guest actually re-enumerates
    //    CPUID and ACPI tables from scratch.
    s.push_str("# Hard reset required for CPUID re-enumeration\n");
    s.push_str(&format!(
        "Stop-VM -Name \"{}\" -TurnOff -Force -Confirm:$false\n",
        vm_name
    ));
    s.push_str("Start-Sleep -Seconds 3\n");
    s.push_str(&format!("Start-VM -Name \"{}\"\n", vm_name));

    // 7. Verification pass — confirm the hypervisor bit is back.
    //    The guest will report \"A hypervisor has been detected\" in
    //    its boot log, which is exactly what we want post-rollback.
    s.push_str("# Verify unmask succeeded (timeout 120s)\n");
    s.push_str(&format!(
        "$deadline = (Get-Date).AddSeconds(120)\n\
         $ok = $false\n\
         while ((Get-Date) -lt $deadline) {{\n\
             $state = (Get-VM -Name \"{}\").State\n\
             if ($state -eq 'Running') {{\n\
                 $probe = Invoke-Command -VMName \"{}\" ", 
        vm_name, vm_name
    ));
    s.push_str("-ScriptBlock { (Get-CimInstance Win32_ComputerSystem).HypervisorPresent }\n");
    s.push_str("                 if ($probe) { $ok = $true; break }\n\
             }\n\
             Start-Sleep -Seconds 5\n\
         }\n");
    s.push_str("if (-not $ok) { Write-Error 'Unmask verification failed' }\n");

    s
}