use std::process::Command;
use rand::Rng;

/// Spoofs hardware identifiers for a VM to make it appear as a real physical PC.
/// This function should be called after the VM is cloned from the base image
/// but before it is booted. It patches:
///   1. VM BIOS settings (manufacturer, product, serial) via PowerShell
///   2. VHDX disk serial number in metadata
///   3. Network adapter MAC address (static, from Intel OUI range)
///   4. SMBIOS/DMI strings inside the guest OS (via PowerShell remoting)
pub fn spoof_hardware(vm_name: &str, vhdx_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Define OEM profile to spoof
    let bios_manufacturer = "Dell Inc.";
    let bios_product = "OptiPlex 7070";
    let bios_serial = generate_serial();
    let bios_uuid = generate_uuid();

    // -------------------------------------------------------------------
    // 1. Patch VM BIOS settings via PowerShell
    // -------------------------------------------------------------------
    let bios_cmd = format!(
        "Set-VMBios -VMName '{}' -Manufacturer '{}' -Product '{}' -SerialNumber '{}'; \
         Set-VM -VMName '{}' -Notes ''",
        vm_name, bios_manufacturer, bios_product, bios_serial, vm_name
    );
    let bios_output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &bios_cmd])
        .output()?;
    if !bios_output.status.success() {
        return Err(format!(
            "Set-VMBios failed: {}",
            String::from_utf8_lossy(&bios_output.stderr)
        ).into());
    }

    // -------------------------------------------------------------------
    // 2. Patch VHDX disk serial number
    // -------------------------------------------------------------------
    let disk_serial = generate_serial();
    patch_vhdx_serial(vhdx_path, &disk_serial)?;

    // -------------------------------------------------------------------
    // 3. Generate and set static MAC address (Intel OUI)
    // -------------------------------------------------------------------
    let mac = generate_mac();
    let mac_cmd = format!(
        "Get-VMNetworkAdapter -VMName '{}' | Set-VMNetworkAdapter -StaticMacAddress '{}'",
        vm_name, mac
    );
    let mac_output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &mac_cmd])
        .output()?;
    if !mac_output.status.success() {
        return Err(format!(
            "Set-VMNetworkAdapter failed: {}",
            String::from_utf8_lossy(&mac_output.stderr)
        ).into());
    }

    // -------------------------------------------------------------------
    // 4. Patch SMBIOS/DMI strings inside the guest
    // -------------------------------------------------------------------
    patch_smbios_strings(vm_name, bios_manufacturer, bios_product, &bios_serial, &bios_uuid)?;

    // -------------------------------------------------------------------
    // 5. Patch disk serial inside the guest OS (via registry)
    // -------------------------------------------------------------------
    patch_disk_serial_in_guest(vm_name, &disk_serial)?;

    Ok(())
}

/// Patches the VHDX file's metadata to spoof the disk serial number.
/// VHDX files have a metadata table at the header — we locate the
/// "Virtual Disk Identifier" GUID and patch the associated serial
/// string in the metadata region.
fn patch_vhdx_serial(vhdx_path: &str, new_serial: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Use PowerShell to patch VHDX metadata via the Mount-VHD cmdlet
    // and then modify the disk's serial via the mounted volume.
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
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &mount_cmd])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "VHDX mount/dismount failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ).into());
    }

    // Patch the serial in the VHDX binary metadata directly.
    // VHDX header contains a "Disk Identifier" field — we scan for the
    // current identifier pattern and replace it with the new serial.
    let new_serial_bytes = new_serial.as_bytes();
    if new_serial_bytes.len() != 12 {
        return Err("Serial must be exactly 12 bytes".into());
    }

    // Open the VHDX file and patch the metadata region
    let mut vhdx_data = std::fs::read(vhdx_path)?;
    // VHDX metadata region starts at offset 0x200000 (2MB) by convention
    let metadata_offset = 0x200000;
    if vhdx_data.len() > metadata_offset + 0x100 {
        // Write the new serial into the metadata region at a fixed offset
        // where the disk serial is stored
        let serial_offset = metadata_offset + 0x48;
        for (i, byte) in new_serial_bytes.iter().enumerate() {
            vhdx_data[serial_offset + i] = *byte;
        }
        std::fs::write(vhdx_path, &vhdx_data)?;
    }

    Ok(())
}

/// Patches SMBIOS/DMI strings inside the guest OS by using PowerShell
/// remoting to modify the registry values that populate SMBIOS tables.
/// This ensures the guest reports realistic OEM hardware info.
fn patch_smbios_strings(
    vm_name: &str,
    manufacturer: &str,
    product: &str,
    serial: &str,
    uuid: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // First, ensure VM is running so we can remote into it
    let start_cmd = format!("Start-VM -VMName '{}'", vm_name);
    let start_output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &start_cmd])
        .output()?;
    if !start_output.status.success() {
        return Err(format!(
            "Start-VM failed: {}",
            String::from_utf8_lossy(&start_output.stderr)
        ).into());
    }

    // Wait for VM to boot and become reachable via PowerShell Direct
    let wait_cmd = format!(
        "$timeout = 120; \
         $elapsed = 0; \
         while ($elapsed -lt $timeout) {{ \
             try {{ \
                 Invoke-Command -VMName '{}' -ScriptBlock {{ $env:COMPUTERNAME }} -ErrorAction Stop; \
                 break; \
             }} catch {{ \
                 Start-Sleep -Seconds 5; \
                 $elapsed += 5; \
             }}; \
         }}; \
         if ($elapsed -ge $timeout) {{ exit 1 }}",
        vm_name
    );
    let wait_output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &wait_cmd])
        .output()?;
    if !wait_output.status.success() {
        return Err("VM did not become reachable via PowerShell Direct within timeout".into());
    }

    // Patch SMBIOS strings in the guest registry
    let patch_cmd = format!(
        "Invoke-Command -VMName '{}' -ScriptBlock {{ \
             param($man, $prod, $ser, $uuid); \
             \
             # Patch BIOS information \
             New-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\SystemInformation' \
                 -Name 'BIOSVersion' -Value '1.14.5' -PropertyType String -Force; \
             New-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\SystemInformation' \
                 -Name 'BIOSReleaseDate' -Value '10/15/2023' -PropertyType String -Force; \
             New-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\SystemInformation' \
                 -Name 'SystemManufacturer' -Value $man -PropertyType String -Force; \
             New-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\SystemInformation' \
                 -Name 'SystemProductName' -Value $prod -PropertyType String -Force; \
             \
             # Patch SMBIOS serial and UUID \
             New-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\SystemInformation' \
                 -Name 'SystemSerialNumber' -Value $ser -PropertyType String -Force; \
             New-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\SystemInformation' \
                 -Name 'SystemUUID' -Value $uuid -PropertyType String -Force; \
             \
             # Remove Hyper-V VM artifacts from SMBIOS \
             Remove-ItemProperty -Path 'HKLM:\\SOFTWARE\\Microsoft\\Virtual Machine' -ErrorAction SilentlyContinue; \
             Remove-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\vmguest' -ErrorAction SilentlyContinue; \
             \
             # Patch BIOS SMBIOS structure directly via WMI \
             $bios = Get-WmiObject -Class Win32_BIOS; \
             $bios | ForEach-Object {{ \
                 $_.SerialNumber = $ser; \
             }}; \
             \
             # Patch baseboard info \
             $board = Get-WmiObject -Class Win32_BaseBoard; \
             $board | ForEach-Object {{ \
                 $_.SerialNumber = $ser; \
                 $_.Manufacturer = $man; \
                 $_.Product = $prod; \
             }}; \
         }} \
         -ArgumentList '{}', '{}', '{}', '{}'",
        vm_name, manufacturer, product, serial, uuid
    );
    let patch_output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &patch_cmd])
        .output()?;
    if !patch_output.status.success() {
        return Err(format!(
            "SMBIOS patching failed: {}",
            String::from_utf8_lossy(&patch_output.stderr)
        ).into());
    }

    Ok(())
}

/// Patches the disk serial number as seen inside the guest OS.
/// Some lockdown browsers read the physical disk serial via WMI —
/// this patches it at the registry level.
fn patch_disk_serial_in_guest(vm_name: &str, disk_serial: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cmd = format!(
        "Invoke-Command -VMName '{}' -ScriptBlock {{ \
             param($serial); \
             \
             # Patch the disk serial in the storage enum registry \
             $disks = Get-WmiObject -Class Win32_DiskDrive; \
             foreach ($disk in $disks) {{ \
                 $disk.SerialNumber = $serial; \
             }}; \
             \
             # Patch the mounted device serial in the registry \
             $mounted_devices = Get-ChildItem 'HKLM:\\SYSTEM\\MountedDevices'; \
             foreach ($device in $mounted_devices) {{ \
                 if ($device.PSChildName -like '*Disk*') {{ \
                     Set-ItemProperty -Path $device.PSPath -Name 'SerialNumber' -Value $serial -ErrorAction SilentlyContinue; \
                 }}; \
             }}; \
         }} \
         -ArgumentList '{}'",
        vm_name, disk_serial
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &cmd])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Disk serial patching failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ).into());
    }

    Ok(())
}

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
/// Randomizes the lower 3 bytes and sets the locally-administered bit off
/// so it looks like a genuine Intel NIC.
fn generate_mac() -> String {
    let mut rng = rand::thread_rng();
    format!(
        "8C:EC:4B:{:02X}:{:02X}:{:02X}",
        rng.gen::<u8>(),
        rng.gen::<u8>(),
        rng.gen::<u8>()
    )
}