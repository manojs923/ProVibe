use std::io::{Error, ErrorKind};
use std::path::PathBuf;
use std::process::Command;

/// Internal switch shared with `vm::mod` and `vm::network`. One definition so
/// the three modules can't drift apart.
pub const LOOPBACK_SWITCH: &str = "Protocol-Loopback";

// hyper-v orchestration module
// spins up the localized matrix and strips the hypervisor signature.

pub struct HyperVOrchestrator {
    vm_name: String,
    switch_name: String,
}

fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn run_ps(script: &str) -> Result<String, Error> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::new(
            ErrorKind::Other,
            format!("ps execution failed: {}", err),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

impl HyperVOrchestrator {
    /// `name` is used verbatim — callers already hold the final name, so
    /// deriving one here made teardown target a VM that never existed.
    pub fn new(name: &str) -> Self {
        Self {
            vm_name: name.to_string(),
            switch_name: LOOPBACK_SWITCH.to_string(),
        }
    }

    pub fn vm_name(&self) -> &str {
        &self.vm_name
    }

    fn execute_ps(&self, script: &str) -> Result<String, Error> {
        run_ps(script)
    }

    fn state(&self) -> Option<String> {
        let script = format!(
            "(Get-VM -Name '{}' -ErrorAction SilentlyContinue).State.ToString()",
            ps_escape(&self.vm_name)
        );
        match run_ps(&script) {
            Ok(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ => None,
        }
    }

    /// Create the guest from a pre-sysprepped parent image. No-op when the VM
    /// already exists, so re-engaging is safe.
    ///
    /// `parent_vhdx` is the base image; a differencing child is created under
    /// ProgramData so the base stays pristine.
    pub fn create_vm(&self, parent_vhdx: &str, ram_mb: u32, cores: u8) -> Result<(), Error> {
        if self.state().is_some() {
            return Ok(());
        }

        let parent = PathBuf::from(parent_vhdx);
        if !parent.exists() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("parent VHDX missing: {}", parent.display()),
            ));
        }

        let bytes = (ram_mb as u64) * 1024 * 1024;
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$name = '{name}'
$base = '{parent}'
$dir  = Join-Path $env:ProgramData 'Protocol\VMs'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$vhd = Join-Path $dir ($name + '.vhdx')
New-VHD -Path $vhd -ParentPath $base -Differencing | Out-Null
New-VM -Name $name -VHDPath $vhd -MemoryStartupBytes {ram} -Generation 2 | Out-Null
Set-VMProcessor -VMName $name -Count {cores}
Set-VM -Name $name -CheckpointType Disabled
"#,
            name = ps_escape(&self.vm_name),
            parent = ps_escape(&parent.to_string_lossy()),
            ram = bytes,
            cores = cores
        );
        self.execute_ps(&script)?;
        Ok(())
    }

    pub fn apply_stealth_patches(&self) -> Result<(), Error> {
        // The optical illusion. `CompatibilityForMigrationEnabled` masks host
        // CPU features from the guest, and nested virt is switched off so
        // `cpuid` leaf 0x40000000 stops advertising a hypervisor.
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
Set-VMProcessor -VMName '{name}' -ExposeVirtualizationExtensions $false
Set-VMProcessor -VMName '{name}' -CompatibilityForMigrationEnabled $true
# SMBIOS string overwrites (Dell/Lenovo spoofing) hook in here via Msvm_ComputerSystem.
"#,
            name = ps_escape(&self.vm_name)
        );
        self.execute_ps(&script)?;
        Ok(())
    }

    pub fn attach_loopback_network(&self) -> Result<(), Error> {
        // Isolate the patient: internal switch only, no route to the internet.
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$switch = Get-VMSwitch -Name '{switch}' -ErrorAction SilentlyContinue
if (-not $switch) {{ New-VMSwitch -Name '{switch}' -SwitchType Internal -Notes 'airgap' | Out-Null }}
$nic = Get-VMNetworkAdapter -VMName '{name}'
if (-not $nic) {{ Add-VMNetworkAdapter -VMName '{name}' -SwitchName '{switch}' | Out-Null }}
else {{ Connect-VMNetworkAdapter -VMName '{name}' -SwitchName '{switch}' }}

$netAdapter = Get-NetAdapter | Where-Object {{ $_.InterfaceDescription -match 'Hyper-V Virtual Ethernet Adapter' -and $_.Name -match '{switch}' }}
if ($netAdapter) {{ New-NetIPAddress -InterfaceAlias $netAdapter.Name -IPAddress 127.0.0.2 -PrefixLength 24 -ErrorAction SilentlyContinue | Out-Null }}
"#,
            switch = ps_escape(&self.switch_name),
            name = ps_escape(&self.vm_name)
        );
        self.execute_ps(&script)?;
        Ok(())
    }

    pub fn start(&self) -> Result<(), Error> {
        if let Some(state) = self.state() {
            if state.eq_ignore_ascii_case("Running") {
                return Ok(());
            }
        }
        self.execute_ps(&format!("Start-VM -Name '{}'", ps_escape(&self.vm_name)))?;
        Ok(())
    }

    pub fn destroy(&self) -> Result<(), Error> {
        let name = ps_escape(&self.vm_name);
        // Pull the disk path before the VM record disappears, then unlink the
        // differencing child. `Remove-VM` alone leaks it in ProgramData.
        let script = format!(
            r#"$ErrorActionPreference = 'SilentlyContinue'
$vm = Get-VM -Name '{name}'
$disks = if ($vm) {{ $vm.HardDrives | ForEach-Object {{ $_.Path }} }} else {{ @() }}
Stop-VM -Name '{name}' -TurnOff -Force
Remove-VM -Name '{name}' -Force
foreach ($d in $disks) {{ if ($d -and (Test-Path $d)) {{ Remove-Item $d -Force }} }}
"#,
            name = name
        );
        self.execute_ps(&script)?;
        Ok(())
    }
}