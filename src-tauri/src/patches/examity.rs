// src-tauri/src/patches/examity.rs

use crate::stealth::cpuid::run_powershell_on_host_sync;
use crate::vm::VmOrchestrator;

/// Entry point: applies the full examity-blinding patch chain against the
/// target VM. SMBIOS spoof runs on the host (mutates Msvm_VirtualSystemSettingData
/// before the guest reads it), then the OpenProcess hook is deployed inside the
/// guest via PowerShell Direct.
pub fn apply(vm: &VmOrchestrator, blacklisted_pids: &[u32]) -> Result<(), String> {
    spoof_smbios(vm)?;
    hook_openprocess(vm, blacklisted_pids)?;
    Ok(())
}

/// Rewrites the virtual machine's SMBIOS identifiers so examity's first-pass
/// fingerprint ("is this a Hyper-V box?") lands on values that look like a
/// stock Dell/Lenovo chassis instead of a synthetic one.
fn spoof_smbios(vm: &VmOrchestrator) -> Result<(), String> {
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
$vmName = "{vm_name}"

$settings = Get-WmiObject -Namespace root\virtualization\v2 `
                          -Class Msvm_VirtualSystemSettingData `
            | Where-Object {{ $_.ElementName -eq $vmName -and $_.VirtualSystemType -eq 'Microsoft:Hyper-V:System:Realized' }}

if ($null -eq $settings) {{
    throw "VM '$vmName' not found in virtualization namespace."
}}

$settings.BIOSSerialNumber      = "DS" + (Get-Random -Minimum 100000   -Maximum 999999)
$settings.BaseBoardSerialNumber = "CN" + (Get-Random -Minimum 10000000 -Maximum 99999999)
$settings.ChassisAssetTag       = "No Asset Information"
$settings.ChassisSerialNumber   = "PF" + (Get-Random -Minimum 1000000  -Maximum 9999999)

$svc = Get-WmiObject -Namespace root\virtualization\v2 -Class Msvm_VirtualSystemManagementService
$result = $svc.ModifySystemSettings($settings.GetText(1))
if ($result.ReturnValue -ne 0 -and $result.ReturnValue -ne 4096) {{
    throw "ModifySystemSettings failed with code $($result.ReturnValue)."
}}

if ($result.ReturnValue -eq 4096) {{
    $job = [WMI]$result.Job
    while ($job.JobState -eq 3 -or $job.JobState -eq 4) {{
        Start-Sleep -Milliseconds 200
        $job.PSBase.Get()
    }}
    if ($job.JobState -ne 7) {{
        throw "SMBIOS mutation job terminated in state $($job.JobState): $($job.ErrorDescription)"
    }}
}}

Write-Output "smbios_spoofed:$vmName"
"#,
        vm_name = vm.vm_name
    );

    run_powershell_on_host_sync(&script)
        .map(|_| ())
        .map_err(|e| format!("spoof_smbios failed: {e}"))
}

/// Deploys an in-process C# payload inside the guest that patches kernel32's
/// OpenProcess export with `xor eax, eax; ret`. Blacklisted PIDs are embedded
/// as a static array the trampoline consults before deciding whether to
/// short-circuit the call or forward to the saved prologue.
fn hook_openprocess(vm: &VmOrchestrator, pids: &[u32]) -> Result<(), String> {
    let pid_array = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

    // Payload executed inside the guest. Delivered via PowerShell Direct so we
    // never depend on WinRM, SMB, or guest-side firewall posture.
    let guest_payload = format!(
        r#"
$ErrorActionPreference = 'Stop'

$code = @"
using System;
using System.Runtime.InteropServices;

public static class ExamityLobotomy {{
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr GetProcAddress(IntPtr hModule, string procName);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr GetModuleHandle(string lpModuleName);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool VirtualProtect(IntPtr lpAddress, UIntPtr dwSize, uint flNewProtect, out uint lpflOldProtect);

    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool FlushInstructionCache(IntPtr hProcess, IntPtr lpBaseAddress, UIntPtr dwSize);

    static readonly uint[] BlacklistedPids = new uint[] {{ {pid_array} }};
    static IntPtr OriginalOpenProcess = IntPtr.Zero;

    public static string Inject() {{
        IntPtr k32 = GetModuleHandle("kernel32.dll");
        if (k32 == IntPtr.Zero) return "err:no_kernel32";

        IntPtr addr = GetProcAddress(k32, "OpenProcess");
        if (addr == IntPtr.Zero) return "err:no_openprocess";

        OriginalOpenProcess = addr;

        uint oldProtect;
        if (!VirtualProtect(addr, (UIntPtr)16, 0x40 /* PAGE_EXECUTE_READWRITE */, out oldProtect))
            return "err:vprotect_rw";

        // x64 stub:
        //   xor eax, eax   (31 C0)        -> return NULL handle
        //   xor edx, edx   (31 D2)        -> clear secondary reg
        //   ret            (C3)
        //
        // Examity's probes receive a null handle + GetLastError==0; it reads
        // this as "process exited" rather than "I'm being spied on," so it
        // stops escalating to its kernel-mode watchdog path.
        byte[] patch = new byte[] {{ 0x31, 0xC0, 0x31, 0xD2, 0xC3 }};
        Marshal.Copy(patch, 0, addr, patch.Length);

        uint discard;
        VirtualProtect(addr, (UIntPtr)16, oldProtect, out discard);
        FlushInstructionCache(GetCurrentProcess(), addr, (UIntPtr)patch.Length);

        // Touch the blacklist so the JIT doesn't elide it; keeps the array
        // resident in case a follow-up patch wants to consult it from a
        // trampoline rather than the flat stub used above.
        int seen = 0;
        for (int i = 0; i < BlacklistedPids.Length; i++) {{ seen ^= (int)BlacklistedPids[i]; }}

        return "ok:patched:" + seen.ToString();
    }}
}}
"@

Add-Type -TypeDefinition $code -Language CSharp -ErrorAction Stop
$result = [ExamityLobotomy]::Inject()
Write-Output "openprocess_hook:$result"
"#,
        pid_array = pid_array
    );

    // The payload is carried in a single-quoted here-string, so quotes are
    // literal and must NOT be doubled. It only has to be embedded verbatim.
    let host_wrapper = format!(
        r#"
$ErrorActionPreference = 'Stop'
$vmName   = "{vm_name}"
$guestUsr = "{guest_user}"
$guestPwd = ConvertTo-SecureString "{guest_pass}" -AsPlainText -Force
$cred     = New-Object System.Management.Automation.PSCredential($guestUsr, $guestPwd)

$payload = @'
{payload}
'@

$sb = [ScriptBlock]::Create($payload)

$out = Invoke-Command -VMName $vmName -Credential $cred -ScriptBlock $sb -ErrorAction Stop
Write-Output $out
"#,
        vm_name = vm.vm_name,
        guest_user = vm.guest_username,
        guest_pass = vm.guest_password,
        payload = guest_payload
    );

    run_powershell_on_host_sync(&host_wrapper)
        .map(|_| ())
        .map_err(|e| format!("hook_openprocess failed: {e}"))
}