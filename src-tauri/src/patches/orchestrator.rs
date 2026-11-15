// src-tauri/src/patches/orchestrator.rs

use std::io::Write;
use std::process::{Command, Stdio};

use crate::hyper_v::{build_guest_invocation, GuestCredentials};
use crate::vm::VmOrchestrator;

/// Run a host PowerShell script over stdin and require success.
fn run_host_ps(script: &str) -> Result<(), String> {
    let mut child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("powershell spawn failed: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(script.as_bytes())
            .map_err(|e| format!("script write failed: {}", e))?;
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("powershell wait failed: {}", e))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "powershell failed".to_string()
        } else {
            err
        });
    }
    Ok(())
}

/// Run a script inside the guest via PowerShell Direct.
fn run_guest_ps(vm: &VmOrchestrator, guest_script: &str) -> Result<(), String> {
    let creds = GuestCredentials::new(&vm.guest_username, &vm.guest_password);
    let host_script = build_guest_invocation(&vm.vm_name, &creds, guest_script);
    run_host_ps(&host_script)
}

pub fn apply(vm: &VmOrchestrator, blacklisted_pids: &[u32]) -> Result<(), String> {
    spoof_smbios(vm)?;
    hook_openprocess(vm, blacklisted_pids)?;
    Ok(())
}

/// Rewrite the guest's BIOS and chassis identity.
///
/// The correct classes are `Msvm_BIOSElement` (SerialNumber) and `Msvm_Chassis`
/// (AssetTag) — `Msvm_VirtualSystemSettingData` carries neither. `Set-CimInstance`
/// drives `ModifySystemSettings` internally; a plain property assignment plus
/// `Put()` does not persist.
///
/// Requires the VM to be **powered off**. Called after boot it will no-op.
fn spoof_smbios(vm: &VmOrchestrator) -> Result<(), String> {
    let name = vm.vm_name.replace('\'', "''");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$name = '{name}'
$vm = Get-CimInstance -Namespace root\virtualization\v2 -Class Msvm_ComputerSystem |
      Where-Object {{ $_.ElementName -eq $name }}
if (-not $vm) {{ throw "vm not found: $name" }}

$sd = Get-CimAssociatedInstance -InputObject $vm -ResultClassName Msvm_VirtualSystemSettingData |
      Where-Object {{ $_.VirtualSystemType -eq 'Microsoft:Hyper-V:System:Realized' }} |
      Select-Object -First 1
if (-not $sd) {{ throw 'realized setting data not found' }}

$serial = 'DS' + (Get-Random -Minimum 100000 -Maximum 999999)
$bios = Get-CimAssociatedInstance -InputObject $sd -ResultClassName Msvm_BIOSElement
if ($bios) {{ $bios | Set-CimInstance -Property @{{ SerialNumber = $serial }} }}

$chassis = Get-CimAssociatedInstance -InputObject $sd -ResultClassName Msvm_Chassis
if ($chassis) {{ $chassis | Set-CimInstance -Property @{{ AssetTag = 'No Asset Information' }} }}

Write-Output "smbios rewritten: $serial"
"#,
        name = name
    );
    run_host_ps(&script)
}

/// Inline-patch `kernel32!OpenProcess` in the guest so blacklisted PIDs look
/// like they cannot be opened.
///
/// The here-string terminator must sit at column 0 — PowerShell rejects an
/// indented `"@`, and it is indented in the generated source.
///
/// Caveat that matters: `Add-Type` compiles into the *PowerShell host process*,
/// so the patch dies when that process exits. To make it durable the same C#
/// has to be injected into the browser process instead. Until then this is
/// best-effort and the PID list is documentation rather than enforcement.
fn hook_openprocess(vm: &VmOrchestrator, pids: &[u32]) -> Result<(), String> {
    let pid_list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let guest_script = format!(
        r#"$ErrorActionPreference = 'Stop'
$code = @"
using System;
using System.Runtime.InteropServices;

public class OpenProcessHook {{
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr GetProcAddress(IntPtr hModule, string procName);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr GetModuleHandle(string lpModuleName);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool VirtualProtect(IntPtr lpAddress, UIntPtr dwSize, uint flNewProtect, out uint lpflOldProtect);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool FlushInstructionCache(IntPtr hProcess, IntPtr lpBaseAddress, UIntPtr dwSize);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();

    // blacklist: {pids}
    public static void Inject() {{
        IntPtr addr = GetProcAddress(GetModuleHandle("kernel32.dll"), "OpenProcess");
        if (addr == IntPtr.Zero) throw new Exception("OpenProcess not resolved");

        uint old;
        if (!VirtualProtect(addr, (UIntPtr)5, 0x40, out old))
            throw new Exception("VirtualProtect failed");

        // xor eax, eax; ret -> every OpenProcess returns NULL.
        byte[] stub = {{ 0x31, 0xC0, 0xC3 }};
        Marshal.Copy(stub, 0, addr, stub.Length);

        uint ignored;
        VirtualProtect(addr, (UIntPtr)5, old, out ignored);
        // Without this other cores can keep executing stale bytes.
        FlushInstructionCache(GetCurrentProcess(), addr, (UIntPtr)stub.Length);
    }}
}}
"@
Add-Type -TypeDefinition $code -Language CSharp
[OpenProcessHook]::Inject()
Write-Output 'OpenProcess blinded'
"#,
        pids = if pid_list.is_empty() { "none".to_string() } else { pid_list }
    );

    run_guest_ps(vm, &guest_script)
}