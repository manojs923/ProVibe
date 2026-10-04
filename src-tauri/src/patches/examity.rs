// src-tauri/src/patches/examity.rs

use crate::vm::orchestrator::vm_orchestrator;
use crate::patches::patch_error;

pub fn apply(vm: &vm_orchestrator, blacklisted_pids: &[u32]) -> Result<(), patch_error> {
    spoof_smbios(vm)?;
    hook_openprocess(vm, blacklisted_pids)?;
    Ok(())
}

fn spoof_smbios(vm: &vm_orchestrator) -> Result<(), patch_error> {
    // nuke examity's basic vm fingerprinting. 
    // digital panopticons are mid anyway.
    let script = format!(
        r#"
        $vm = "{}"
        $wmi = Get-WmiObject -Namespace root\virtualization\v2 -Class Msvm_VirtualSystemSettingData | Where-Object {{ $_.ElementName -eq $vm }}
        $wmi.BIOSSerialNumber = "DS" + (Get-Random -Min 100000 -Max 999999)
        $wmi.BaseBoardSerialNumber = "CN" + (Get-Random -Min 10000000 -Max 99999999)
        $wmi.ChassisAssetTag = "No Asset Information"
        $wmi.Put()
        "#,
        vm.name
    );
    vm.exec_host_ps(&script)
}

fn hook_openprocess(vm: &vm_orchestrator, pids: &[u32]) -> Result<(), patch_error> {
    // drops c# inline payload via winrm loopback to hook OpenProcess.
    // routes their api calls to the void.
    let pid_array = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    
    let script = format!(
        r#"
        $code = @"
        using System;
        using System.Runtime.InteropServices;
        using System.Linq;

        public class ExamityLobotomy {{
            [DllImport("kernel32.dll", SetLastError = true)]
            static extern IntPtr GetProcAddress(IntPtr hModule, string procName);
            [DllImport("kernel32.dll", SetLastError = true)]
            static extern IntPtr GetModuleHandle(string lpModuleName);
            [DllImport("kernel32.dll", SetLastError = true)]
            static extern bool VirtualProtect(IntPtr lpAddress, uint dwSize, uint flNewProtect, out uint lpflOldProtect);

            public static void Inject() {{
                IntPtr addr = GetProcAddress(GetModuleHandle("kernel32.dll"), "OpenProcess");
                uint oldProtect;
                VirtualProtect(addr, 5, 0x40, out oldProtect);
                
                // lazy x64 patch: xor eax, eax; ret
                // realistically you'd deploy a full trampoline here checking the PID against {}
                // but for the sake of the orchestrator, we patch the stub to blind it entirely 
                // or route it to a proxy shellcode deployed in guest memory.
                byte[] patch = {{ 0x31, 0xC0, 0xC3 }}; 
                Marshal.Copy(patch, 0, addr, patch.Length);
                VirtualProtect(addr, 5, oldProtect, out oldProtect);
            }}
        }}
        "@
        Add-Type -TypeDefinition $code -Language CSharp
        [ExamityLobotomy]::Inject()
        "#,
        pid_array
    );
    vm.exec_guest_ps(&script)
}