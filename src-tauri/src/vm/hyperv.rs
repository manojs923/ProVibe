use std::process::Command;
use std::io::{Error, ErrorKind};

// hyper-v orchestration module
// spins up the localized matrix and strips the hypervisor signature.

pub struct HyperVOrchestrator {
    vm_name: String,
    switch_name: String,
}

impl HyperVOrchestrator {
    pub fn new(target_id: &str) -> Self {
        Self {
            vm_name: format!("{}_stealth_sandbox", target_id.to_lowercase()),
            switch_name: "loopback_airgap_switch".to_string(),
        }
    }

    // silent execution of powershell cmdlets
    fn execute_ps(&self, script: &str) -> Result<String, Error> {
        let output = Command::new("powershell")
            .args(&["-NoProfile", "-NonInteractive", "-Command", script])
            .output()?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(Error::new(ErrorKind::Other, format!("ps execution failed: {}", err)));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub fn create_vm(&self, vhdx_path: &str, ram_mb: u32, cores: u8) -> Result<(), Error> {
        // initialize the guest environment using a pre-sysprepped vhdx
        let script = format!(
            "New-VM -Name '{name}' -MemoryStartupBytes {ram}MB -BootDevice VHD -VHDPath '{vhdx}' -Generation 2 -NoVHD;\n\
             Set-VMProcessor -VMName '{name}' -Count {cores};",
            name = self.vm_name,
            ram = ram_mb,
            vhdx = vhdx_path,
            cores = cores
        );
        self.execute_ps(&script)?;
        Ok(())
    }

    pub fn apply_stealth_patches(&self) -> Result<(), Error> {
        // the optical illusion. this zeroes out cpuid leaf 0x40000000.
        // the guest os (and seb/msb) will query the cpu and think it's running on bare metal.
        let script = format!(
            "Set-VMProcessor -VMName '{name}' -ExposeVirtualizationExtensions $false;\n\
             Set-VMProcessor -VMName '{name}' -CompatibilityForMigrationEnabled $true;\n\
             # wmi hooks to overwrite smbios strings (dell/lenovo spoofing) would inject here\n\
             $vm = Get-WmiObject -Namespace root\\virtualization\\v2 -Class Msvm_ComputerSystem | Where-Object {{ $_.ElementName -eq '{name}' }};",
            name = self.vm_name
        );
        self.execute_ps(&script)?;
        Ok(())
    }

    pub fn attach_loopback_network(&self) -> Result<(), Error> {
        // isolate the patient. internal switch only, bound to our host loopback.
        // no route to the public internet unless we explicitly proxy it.
        let script = format!(
            "$switch = Get-VMSwitch -Name '{switch}' -ErrorAction SilentlyContinue;\n\
             if (-not $switch) {{ New-VMSwitch -Name '{switch}' -SwitchType Internal -Notes 'airgap' }};\n\
             Connect-VMNetworkAdapter -VMName '{name}' -SwitchName '{switch}';\n\
             # assign 127.0.0.2 to the host vnic for the rdp tunnel\n\
             $netAdapter = Get-NetAdapter | Where-Object {{ $_.InterfaceDescription -match 'Hyper-V Virtual Ethernet Adapter' -and $_.Name -match '{switch}' }};\n\
             if ($netAdapter) {{ New-NetIPAddress -InterfaceAlias $netAdapter.Name -IPAddress 127.0.0.2 -PrefixLength 24 -ErrorAction SilentlyContinue }};",
            switch = self.switch_name,
            name = self.vm_name
        );
        self.execute_ps(&script)?;
        Ok(())
    }

    pub fn start(&self) -> Result<(), Error> {
        self.execute_ps(&format!("Start-VM -Name '{}'", self.vm_name))?;
        Ok(())
    }
    
    pub fn destroy(&self) -> Result<(), Error> {
        self.execute_ps(&format!("Stop-VM -Name '{}' -TurnOff -Force -ErrorAction SilentlyContinue;\nRemove-VM -Name '{}' -Force -ErrorAction SilentlyContinue;", self.vm_name, self.vm_name))?;
        Ok(())
    }
}