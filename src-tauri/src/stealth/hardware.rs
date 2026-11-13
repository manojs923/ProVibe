use std::process::Command;
use rand::Rng;
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct HardwareProfile {
    pub bios_manufacturer: String,
    pub bios_product: String,
    pub bios_serial_number: String,
    pub bios_uuid: String,
    pub bios_version: String,
    pub bios_release_date: String,
    pub disks: Vec<DiskProfile>,
    pub nics: Vec<NicProfile>,
}

#[derive(Debug, Clone)]
pub struct DiskProfile {
    pub device_path: String,
    pub serial_number: String,
}

#[derive(Debug, Clone)]
pub struct NicProfile {
    pub name: String,
    pub mac_address: String,
}

pub struct HardwarePersona {
    pub profile: HardwareProfile,
}

impl Default for HardwarePersona {
    fn default() -> Self {
        Self {
            profile: generate_hardware_profile(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TargetProfile;

#[derive(Debug, Clone)]
pub struct VmHandle {
    pub vm_name: String,
    pub vhdx_path: String,
}

impl VmHandle {
    /// Creates a handle from a VM name and the path to its VHDX disk image.
    pub fn new(vm_name: impl Into<String>, vhdx_path: impl Into<String>) -> Self {
        Self {
            vm_name: vm_name.into(),
            vhdx_path: vhdx_path.into(),
        }
    }

    /// The VM's name, as registered with the hypervisor.
    pub fn name(&self) -> &str {
        &self.vm_name
    }

    /// Path to the VM's VHDX disk image.
    pub fn vhdx_path(&self) -> &Path {
        Path::new(&self.vhdx_path)
    }
}

impl VmHandle {
    pub fn open(vm_name: impl Into<String>) -> Result<Self, String> {
        Ok(Self::new(vm_name, String::new()))
    }
}

impl fmt::Display for VmHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.vm_name, self.vhdx_path)
    }
}

#[derive(Debug)]
pub struct StealthError(pub String);

impl std::fmt::Display for StealthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StealthError {}

impl From<String> for StealthError {
    fn from(s: String) -> Self {
        StealthError(s)
    }
}

impl From<&str> for StealthError {
    fn from(s: &str) -> Self {
        StealthError(s.to_string())
    }
}

/// Spoofs hardware identifiers for a VM to make it appear as a real physical PC.
/// This function should be called after the VM is cloned from the base image
/// but before it is booted. It patches:
///   1. VM BIOS settings (manufacturer, product, serial) via PowerShell
///   2. VHDX disk serial number in metadata
///   3. Network adapter MAC address (static, from Intel OUI range)
///   4. SMBIOS/DMI strings inside the guest OS (via PowerShell remoting)
///   5. Disk serial inside the guest OS (via registry)
pub async fn spoof_hardware(
    vm_name: &str,
    vhdx_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let persona = select_persona(&TargetProfile);
    apply_spoof(
        VmHandle {
            vm_name: vm_name.to_string(),
            vhdx_path: vhdx_path.to_string(),
        },
        &persona,
    )
    .await?;
    Ok(())
}

/// Selects a hardware persona for the given target profile.
pub fn select_persona(_target: &TargetProfile) -> HardwarePersona {
    HardwarePersona::default()
}

/// Returns whether the current host supports hardware spoofing
/// (requires Windows with Hyper-V PowerShell cmdlets available).
pub fn host_supports_spoofing() -> bool {
    if cfg!(not(windows)) {
        return false;
    }
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "if (Get-Command Get-VM -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }",
        ])
        .output();
    match output {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

/// Applies the full hardware spoof to a running/cloned VM.
pub async fn apply_spoof(
    vm_handle: VmHandle,
    persona: &HardwarePersona,
) -> Result<(), StealthError> {
    let profile = &persona.profile;
    let vm_name = &vm_handle.vm_name;
    let vhdx_path = &vm_handle.vhdx_path;

    // 1. Patch VM BIOS settings via Hyper-V PowerShell
    patch_vm_bios(vm_name, profile).map_err(|e| StealthError(e.to_string()))?;

    // 2. Patch VHDX disk serial number in metadata
    patch_vhdx_serial(vhdx_path, &profile.disks[0].serial_number)
        .map_err(|e| StealthError(e.to_string()))?;

    // 3. Generate and set static MAC address (Intel OUI)
    patch_mac_addresses(vm_name, profile).await?;

    // 4. Patch SMBIOS/DMI strings inside the guest
    patch_smbios_strings(vm_name, profile).await?;

    // 5. Patch disk serial inside the guest OS (via registry)
    patch_disk_serial_in_guest(vm_name, &profile.disks[0].serial_number)
        .map_err(|e| StealthError(e.to_string()))?;

    Ok(())
}

/// Generates a realistic-looking hardware profile for an OEM system.
fn generate_hardware_profile() -> HardwareProfile {
    HardwareProfile {
        bios_manufacturer: "Dell Inc.".to_string(),
        bios_product: "OptiPlex 7070".to_string(),
        bios_serial_number: generate_serial(),
        bios_uuid: generate_uuid(),
        bios_version: "1.14.5".to_string(),
        bios_release_date: "10/15/2023".to_string(),
        disks: vec![DiskProfile {
            device_path: r"\\.\PHYSICALDRIVE0".to_string(),
            serial_number: generate_serial(),
        }],
        nics: vec![NicProfile {
            name: "Ethernet".to_string(),
            mac_address: generate_mac(),
        }],
    }
}

/// Runs a PowerShell command on the host, returning stderr on failure.
fn run_powershell_host(script: &str) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "PowerShell command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

/// Runs a PowerShell script inside the guest via PowerShell Direct
/// (Invoke-Command -VMName). The VM must be running.
fn run_powershell_in_guest(
    vm_name: &str,
    script: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let escaped = script.replace('\'', "''");
    let full = format!(
        "Invoke-Command -VMName '{}' -ScriptBlock {{ {} }} -ErrorAction Stop",
        vm_name, escaped
    );
    run_powershell_host(&full)
}

/// Runs a PowerShell script inside the guest with an argument list.
fn run_powershell_in_guest_args(
    vm_name: &str,
    script: &str,
    args: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    let escaped = script.replace('\'', "''");
    let arg_list = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let full = format!(
        "Invoke-Command -VMName '{}' -ScriptBlock {{ {} }} -ArgumentList ({}) -ErrorAction Stop",
        vm_name, escaped, arg_list
    );
    run_powershell_host(&full)
}

/// Waits for the VM to become reachable via PowerShell Direct.
fn wait_for_vm_ready(vm_name: &str, timeout_secs: u32) -> Result<(), Box<dyn std::error::Error>> {
    let wait_cmd = format!(
        "$timeout = {timeout}; \
         $elapsed = 0; \
         while ($elapsed -lt $timeout) {{ \
             try {{ \
                 Invoke-Command -VMName '{vm}' -ScriptBlock {{ $env:COMPUTERNAME }} -ErrorAction Stop; \
                 break; \
             }} catch {{ \
                 Start-Sleep -Seconds 5; \
                 $elapsed += 5; \
             }}; \
         }}; \
         if ($elapsed -ge $timeout) {{ exit 1 }}",
        timeout = timeout_secs,
        vm = vm_name
    );
    run_powershell_host(&wait_cmd)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 1. Patch VM BIOS settings via Hyper-V PowerShell
// ---------------------------------------------------------------------------
fn patch_vm_bios(
    vm_name: &str,
    profile: &HardwareProfile,
) -> Result<(), Box<dyn std::error::Error>> {
    let bios_cmd = format!(
        "Set-VMBios -VMName '{}' -Manufacturer '{}' -Product '{}' -SerialNumber '{}'; \
         Set-VM -VMName '{}' -Notes ''",
        vm_name,
        profile.bios_manufacturer,
        profile.bios_product,
        profile.bios_serial_number,
        vm_name
    );
    run_powershell_host(&bios_cmd)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. Patch VHDX disk serial number
// ---------------------------------------------------------------------------
/// Patches the VHDX file's metadata to spoof the disk serial number.
fn patch_vhdx_serial(
    vhdx_path: &str,
    new_serial: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Mount/dismount cycle so the disk object exists with a stable identifier.
    let mount_cmd = format!(
        "Mount-VHD -Path '{}' -NoDriveLetter -Passthru | \
         Get-Disk | \
         ForEach-Object {{ \
             $disk = $_; \
             $disk | Set-Disk -IsOffline $true; \
         }}; \
         Dismount-VHD -Path '{}'",
        vhdx_path, vhdx_path
    );
    run_powershell_host(&mount_cmd)?;

    // Patch the serial in the VHDX binary metadata directly.
    // The VHDX header contains a "Disk Identifier" field — we replace the
    // stored serial bytes with the new serial.
    let new_serial_bytes = new_serial.as_bytes();
    if new_serial_bytes.len() != 12 {
        return Err("Serial must be exactly 12 bytes".into());
    }

    let mut vhdx_data = std::fs::read(vhdx_path)?;
    // VHDX metadata region conventionally starts at offset 0x200000 (2MB).
    let metadata_offset = 0x200000;
    if vhdx_data.len() > metadata_offset + 0x100 {
        let serial_offset = metadata_offset + 0x48;
        for (i, byte) in new_serial_bytes.iter().enumerate() {
            vhdx_data[serial_offset + i] = *byte;
        }
        std::fs::write(vhdx_path, &vhdx_data)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 3. Patch MAC addresses
// ---------------------------------------------------------------------------
/// Sets static MAC addresses on all VM network adapters via Hyper-V,
/// and additionally sets the NIC's locally-administered "Network Address"
/// property inside the guest for parity.
pub async fn patch_mac_addresses(
    vm_name: &str,
    profile: &HardwareProfile,
) -> Result<(), StealthError> {
    for nic in &profile.nics {
        // Host side: static MAC via Hyper-V.
        let mac_cmd = format!(
            "Get-VMNetworkAdapter -VMName '{}' | Where-Object {{ $_.Name -eq '{}' }} | \
             Set-VMNetworkAdapter -StaticMacAddress '{}'",
            vm_name, nic.name, nic.mac_address
        );
        run_powershell_host(&mac_cmd).map_err(|e| StealthError(e.to_string()))?;

        // Guest side: spoof via advanced property (no separators, 12 hex digits).
        let guest_script = format!(
            "$adapter = Get-NetAdapter | Where-Object {{ $_.Name -eq '{}' }}; \
             if ($adapter) {{ \
                 Set-NetAdapterAdvancedProperty -Name $adapter.Name -DisplayName 'Network Address' -DisplayValue '{}' -NoRestart; \
             }}",
            nic.name,
            nic.mac_address.replace(':', "")
        );
        run_powershell_in_guest(vm_name, &guest_script)
            .map_err(|e| StealthError(e.to_string()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 4. Patch SMBIOS/DMI strings inside the guest
// ---------------------------------------------------------------------------
/// Patches SMBIOS/DMI strings inside the guest OS by using PowerShell
/// Direct to modify the registry values that populate SMBIOS tables.
pub async fn patch_smbios_strings(
    vm_name: &str,
    profile: &HardwareProfile,
) -> Result<(), StealthError> {
    // Ensure the VM is running so we can remote into it.
    let start_cmd = format!("Start-VM -VMName '{}'", vm_name);
    run_powershell_host(&start_cmd).map_err(|e| StealthError(e.to_string()))?;

    // Wait for the guest to become reachable via PowerShell Direct.
    wait_for_vm_ready(vm_name, 120).map_err(|e| StealthError(e.to_string()))?;

    // Patch SMBIOS strings in the guest registry.
    let patch_cmd = r#"
        param($man, $prod, $ser, $uuid, $ver, $date);

        # Patch BIOS information
        New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\SystemInformation' `
            -Name 'BIOSVersion' -Value $ver -PropertyType String -Force;
        New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\SystemInformation' `
            -Name 'BIOSReleaseDate' -Value $date -PropertyType String -Force;
        New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\SystemInformation' `
            -Name 'SystemManufacturer' -Value $man -PropertyType String -Force;
        New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\SystemInformation' `
            -Name 'SystemProductName' -Value $prod -PropertyType String -Force;

        # Patch SMBIOS serial and UUID
        New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\SystemInformation' `
            -Name 'SystemSerialNumber' -Value $ser -PropertyType String -Force;
        New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\SystemInformation' `
            -Name 'SystemUUID' -Value $uuid -PropertyType String -Force;

        # Remove Hyper-V VM artifacts from SMBIOS
        Remove-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Virtual Machine' `
            -ErrorAction SilentlyContinue;
        Remove-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Services\vmguest' `
            -ErrorAction SilentlyContinue;

        # Patch baseboard info
        $board = Get-WmiObject -Class Win32_BaseBoard;
        $board | ForEach-Object {
            $_.SerialNumber = $ser;
            $_.Manufacturer = $man;
            $_.Product = $prod;
        };
    "#;

    run_powershell_in_guest_args(
        vm_name,
        patch_cmd,
        &[
            &profile.bios_manufacturer,
            &profile.bios_product,
            &profile.bios_serial_number,
            &profile.bios_uuid,
            &profile.bios_version,
            &profile.bios_release_date,
        ],
    )
    .map_err(|e| StealthError(e.to_string()))?;

    // Additionally patch WMI BIOS instance directly.
    let wmi_script = format!(
        r#"
        Set-WmiInstance -Class Win32_BIOS -Arguments @{{
            SerialNumber = '{}'
            SMBIOSBIOSVersion = '{}'
            ReleaseDate = '{}'
        }}
        "#,
        profile.bios_serial_number, profile.bios_version, profile.bios_release_date
    );
    run_powershell_in_guest(vm_name, &wmi_script)
        .map_err(|e| StealthError(e.to_string()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// 5. Patch disk serial inside the guest OS (via registry)
// ---------------------------------------------------------------------------
/// Patches the disk serial number as seen inside the guest OS.
/// Some lockdown browsers read the physical disk serial via WMI —
/// this patches it at the registry level.
fn patch_disk_serial_in_guest(
    vm_name: &str,
    disk_serial: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cmd = r#"
        param($serial);

        # Patch the disk serial in the storage enum registry
        $disks = Get-WmiObject -Class Win32_DiskDrive;
        foreach ($disk in $disks) {
            $disk.SerialNumber = $serial;
        };

        # Patch the mounted device serial in the registry
        $mounted_devices = Get-ChildItem 'HKLM:\SYSTEM\MountedDevices';
        foreach ($device in $mounted_devices) {
            if ($device.PSChildName -like '*Disk*') {
                Set-ItemProperty -Path $device.PSPath -Name 'SerialNumber' `
                    -Value $serial -ErrorAction SilentlyContinue;
            };
        };
    "#;

    run_powershell_in_guest_args(vm_name, cmd, &[disk_serial])?;
    Ok(())
}

/// Patches all disk serials in the profile (new-code-style API, per-disk).
pub async fn patch_disk_serials(
    vm_name: &str,
    profile: &HardwareProfile,
) -> Result<(), StealthError> {
    for disk in &profile.disks {
        let script = format!(
            r#"
            $disk = Get-WmiObject Win32_DiskDrive | Where-Object {{ $_.DeviceID -eq '{}' }}
            $disk.SerialNumber = '{}'
            "#,
            disk.device_path, disk.serial_number
        );
        run_powershell_in_guest(vm_name, &script).map_err(|e| StealthError(e.to_string()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Restore functions (previously TODOs — now implemented)
// ---------------------------------------------------------------------------
/// Restores the VM BIOS to Hyper-V defaults.
pub async fn restore_smbios_profile(
    vm_name: &str,
) -> Result<(), StealthError> {
    let bios_cmd = format!(
        "Set-VMBios -VMName '{}' -Manufacturer 'Microsoft Corporation' \
         -Product 'Virtual Machine' -SerialNumber '0000-0000-0000-0000'; \
         Set-VM -VMName '{}' -Notes ''",
        vm_name, vm_name
    );
    run_powershell_host(&bios_cmd).map_err(|e| StealthError(e.to_string()))?;

    // Revert guest registry SystemInformation values to Hyper-V defaults.
    let script = r#"
        $si = 'HKLM:\SYSTEM\CurrentControlSet\Control\SystemInformation';
        Set-ItemProperty -Path $si -Name 'SystemManufacturer' -Value 'Microsoft Corporation';
        Set-ItemProperty -Path $si -Name 'SystemProductName' -Value 'Virtual Machine';
        Set-ItemProperty -Path $si -Name 'SystemSerialNumber' -Value '0000-0000-0000-0000';
        Set-ItemProperty -Path $si -Name 'BIOSVersion' -Value 'Hyper-V';
        Set-ItemProperty -Path $si -Name 'BIOSReleaseDate' -Value '01/01/2000';
    "#;
    run_powershell_in_guest(vm_name, script).map_err(|e| StealthError(e.to_string()))?;

    // Also reset the WMI BIOS instance to generic Hyper-V defaults.
    let wmi_script = r#"
        Set-WmiInstance -Class Win32_BIOS -Arguments @{
            SerialNumber = '0000-0000-0000-0000'
            SMBIOSBIOSVersion = 'Hyper-V'
            ReleaseDate = '01/01/2000'
        }
    "#;
    run_powershell_in_guest(vm_name, wmi_script).map_err(|e| StealthError(e.to_string()))?;

    Ok(())
}

/// Restores disk serial numbers in the guest OS to a neutral default.
/// Note: Hyper-V-generated disks have ephemeral WMI serials, so this
/// simply clears the registry-stored spoofed serials and lets Windows
/// re-enumerate on next boot.
pub async fn restore_disk_serials(vm_name: &str) -> Result<(), StealthError> {
    let script = r#"
        # Clear spoofed serials from mounted devices registry
        $mounted_devices = Get-ChildItem 'HKLM:\SYSTEM\MountedDevices' -ErrorAction SilentlyContinue;
        foreach ($device in $mounted_devices) {
            if ($device.PSChildName -like '*Disk*') {
                Remove-ItemProperty -Path $device.PSPath -Name 'SerialNumber' `
                    -ErrorAction SilentlyContinue;
            };
        };

        # Reset any WMI-visible disk serials to blank so Windows re-enumerates
        $disks = Get-WmiObject -Class Win32_DiskDrive;
        foreach ($disk in $disks) {
            $disk.SerialNumber = '';
        };
    "#;
    run_powershell_in_guest(vm_name, script).map_err(|e| StealthError(e.to_string()))?;
    Ok(())
}

/// Restores all VM network adapters to dynamically-assigned MAC addresses
/// and clears any guest-side "Network Address" overrides.
pub async fn restore_mac_addresses(vm_name: &str) -> Result<(), StealthError> {
    // Host side: switch every NIC back to dynamic MAC assignment.
    let host_cmd = format!(
        "Get-VMNetworkAdapter -VMName '{}' | Set-VMNetworkAdapter -DynamicMacAddress",
        vm_name
    );
    run_powershell_host(&host_cmd).map_err(|e| StealthError(e.to_string()))?;

    // Guest side: clear the locally-administered Network Address override on every NIC.
    let guest_script = r#"
        $adapters = Get-NetAdapter -ErrorAction SilentlyContinue;
        foreach ($adapter in $adapters) {
            try {
                Set-NetAdapterAdvancedProperty -Name $adapter.Name `
                    -DisplayName 'Network Address' -DisplayValue '' -NoRestart `
                    -ErrorAction SilentlyContinue;
            } catch { }
        };
    "#;
    run_powershell_in_guest(vm_name, guest_script).map_err(|e| StealthError(e.to_string()))?;

    Ok(())
}

/// Convenience: full restore in one call.
pub async fn restore_hardware(vm_name: &str) -> Result<(), StealthError> {
    restore_smbios_profile(vm_name).await?;
    restore_disk_serials(vm_name).await?;
    restore_mac_addresses(vm_name).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Random identifier generators
// ---------------------------------------------------------------------------

/// Generates a realistic-looking 12-character alphanumeric serial number
/// in the style of Dell/Lenovo serials (uppercase alphanumeric).
fn generate_serial() -> String {
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| {
            let idx = rng.gen_range(0..36);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'A' + (idx - 10)) as char
            }
        })
        .collect()
}

/// Generates a realistic-looking UUID v4 string for SMBIOS System UUID.
fn generate_uuid() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes);
    // Set version 4 and variant bits
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Generates a random MAC address from Intel OUI range (0x8C:0xEC:0x4B).
/// Randomizes the lower 3 bytes so it looks like a genuine Intel NIC.
fn generate_mac() -> String {
    let mut rng = rand::thread_rng();
    format!(
        "8C:EC:4B:{:02X}:{:02X}:{:02X}",
        rng.gen::<u8>(),
        rng.gen::<u8>(),
        rng.gen::<u8>()
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_is_twelve_uppercase_alnum() {
        let s = generate_serial();
        assert_eq!(s.len(), 12);
        assert!(s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn uuid_has_correct_format() {
        let u = generate_uuid();
        assert_eq!(u.len(), 36);
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // Version 4 nibble
        assert!(parts[2].starts_with('4'));
    }

    #[test]
    fn mac_starts_with_intel_oui() {
        let m = generate_mac();
        assert!(m.starts_with("8C:EC:4B:"));
        assert_eq!(m.len(), 17);
    }

    #[test]
    fn hardware_profile_populates_all_fields() {
        let p = generate_hardware_profile();
        assert_eq!(p.bios_manufacturer, "Dell Inc.");
        assert_eq!(p.bios_product, "OptiPlex 7070");
        assert_eq!(p.bios_serial_number.len(), 12);
        assert_eq!(p.bios_uuid.len(), 36);
        assert!(!p.disks.is_empty());
        assert!(!p.nics.is_empty());
    }

    #[test]
    fn persona_default_has_profile() {
        let persona = HardwarePersona::default();
        assert_eq!(persona.profile.bios_manufacturer, "Dell Inc.");
    }
}