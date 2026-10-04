//! Artifact cleanup — removes VM-specific registry keys
//! (HKLM\SYSTEM\CurrentControlSet\Services\vmguest,
//!  HKLM\SOFTWARE\Microsoft\Virtual Machine),
//! uninstalls Hyper-V Integration Services, and renames the virtual
//! display adapter in the Device Manager registry to match physical
//! GPU entries.

use rand::Rng;

/// Keys that scream "I'm a VM" if left behind.
const VM_REGISTRY_KEYS: &[&str] = &[
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmguest",
    r"HKLM\SOFTWARE\Microsoft\Virtual Machine",
    // Bonus sweep — common leftovers
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmicheartbeat",
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmicvss",
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmicshutdown",
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmicrdv",
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmicguestinterface",
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmictimesync",
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmickvpexchange",
    r"HKLM\SYSTEM\CurrentControlSet\Services\Hypervideo",
    r"HKLM\SOFTWARE\Microsoft\Virtual Machine\Guest",
];

/// Physical GPU entries to impersonate — DeviceDescription values
/// pulled from real hardware inf files.
const GPU_PROFILES: &[(&str, &str)] = &[
    // (DriverDesc, MatchingDeviceId-style hardware string)
    ("NVIDIA GeForce RTX 3070", "PCI\\VEN_10DE&DEV_2484"),
    ("NVIDIA GeForce RTX 3060", "PCI\\VEN_10DE&DEV_2503"),
    ("AMD Radeon RX 6700 XT", "PCI\\VEN_1002&DEV_73DF"),
    ("Intel(R) UHD Graphics 630", "PCI\\VEN_8086&DEV_9BC5"),
    ("Intel(R) Iris(R) Xe Graphics", "PCI\\VEN_8086&DEV_46A6"),
];

/// Virtual display adapter identifiers — what the VM exposes by default.
const VIRTUAL_ADAPTER_MATCHES: &[&str] = &[
    "Microsoft Hyper-V Video",
    "Microsoft Basic Display Adapter",
    "Virtual Display Adapter",
];

/// Result of the artifact cleanup pass.
#[derive(Debug, Clone)]
pub struct CleanupReport {
    pub registry_keys_removed: Vec<String>,
    pub registry_keys_missing: Vec<String>,
    pub registry_keys_failed: Vec<String>,
    pub integration_services_uninstalled: bool,
    pub display_adapter_renamed: Option<String>,
    pub errors: Vec<String>,
}

impl CleanupReport {
    fn new() -> Self {
        Self {
            registry_keys_removed: Vec::new(),
            registry_keys_missing: Vec::new(),
            registry_keys_failed: Vec::new(),
            integration_services_uninstalled: false,
            display_adapter_renamed: None,
            errors: Vec::new(),
        }
    }
}

/// Runs a PowerShell command inside the guest VM via PowerShell Direct.
/// Returns (stdout, stderr) on success.
fn run_in_guest(vm_name: &str, script: &str) -> Result<(String, String), String> {
    let encoded = base64_encode(script.as_bytes());
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Invoke-Command -VMName '{}' -ScriptBlock {{ \
                 ([scriptblock]::Create([System.Text.Encoding]::Unicode.GetString([Convert]::FromBase64String(\"{}\")))) }} \
                 -ErrorAction Stop",
                vm_name, encoded
            ),
        ])
        .output()
        .map_err(|e| format!("failed to spawn powershell: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() && stderr.contains("Exception") {
        return Err(format!("guest PS error: {stderr}"));
    }
    Ok((stdout, stderr))
}

/// UTF-16LE base64 encoding — PowerShell Direct expects this for -EncodedCommand.
fn base64_encode(data: &[u8]) -> String {
    // PowerShell's FromBase64String + Unicode.GetString expects UTF-16LE.
    let mut utf16 = Vec::with_capacity(data.len() * 2);
    for &b in data {
        utf16.push(b);
        utf16.push(0);
    }
    // If the script was already UTF-8, we need proper conversion instead.
    // Simplest correct path: re-encode the original string as UTF-16LE.
    // Since we get bytes here, assume the caller passed UTF-8 text.
    let text = String::from_utf8_lossy(data).to_string();
    utf16.clear();
    for unit in text.encode_utf16() {
        utf16.push((unit & 0xFF) as u8);
        utf16.push((unit >> 8) as u8);
    }
    base64::encode(&utf16)
}

/// Base64 without pulling the crate — keeps Cargo.toml clean.
mod base64 {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
        }
        out
    }
}

/// Picks a random GPU profile for this run so no two VMs look identical.
fn pick_gpu_profile() -> &'static (&'static str, &'static str) {
    let mut rng = rand::thread_rng();
    let idx = rng.gen_range(0..GPU_PROFILES.len());
    &GPU_PROFILES[idx]
}

/// Step 1 — Delete all VM-identifying registry keys inside the guest.
fn scrub_registry(vm_name: &str, report: &mut CleanupReport) {
    for key in VM_REGISTRY_KEYS {
        let script = format!(
            "if (Test-Path 'Microsoft.PowerShell.Core\\Registry::{key}') {{ \
                 Remove-Item -Path 'Microsoft.PowerShell.Core\\Registry::{key}' -Recurse -Force; \
                 Write-Output 'REMOVED' \
             }} else {{ Write-Output 'MISSING' }}"
        );
        match run_in_guest(vm_name, &script) {
            Ok((stdout, _)) => {
                if stdout.contains("REMOVED") {
                    report.registry_keys_removed.push((*key).to_string());
                } else {
                    report.registry_keys_missing.push((*key).to_string());
                }
            }
            Err(e) => {
                report.registry_keys_failed.push(format!("{key}: {e}"));
            }
        }
    }
}

/// Step 2 — Uninstall Hyper-V Integration Services.
/// Modern guests have it baked in as a Windows optional feature, so we
/// try the MSI path first (legacy) then the DISM path.
fn uninstall_integration_services(vm_name: &str, report: &mut CleanupReport) {
    let script = r#"
        $msi = Get-WmiObject Win32_Product -Filter "Name LIKE '%Integration%'" -ErrorAction SilentlyContinue
        if ($msi) {
            $msi | ForEach-Object { $_.Uninstall() | Out-Null }
            Write-Output 'MSI-UNINSTALLED'
        }
        $feat = Get-WindowsOptionalFeature -Online -FeatureName 'GuestServices' -ErrorAction SilentlyContinue
        if ($feat -and $feat.State -eq 'Enabled') {
            Disable-WindowsOptionalFeature -Online -FeatureName 'GuestServices' -NoRestart | Out-Null
            Write-Output 'FEATURE-DISABLED'
        }
        Write-Output 'DONE'
    "#;

    match run_in_guest(vm_name, script) {
        Ok((stdout, _)) => {
            report.integration_services_uninstalled =
                stdout.contains("MSI-UNINSTALLED") || stdout.contains("FEATURE-DISABLED") || stdout.contains("DONE");
        }
        Err(e) => {
            report.errors.push(format!("integration services uninstall failed: {e}"));
        }
    }
}

/// Step 3 — Rename the virtual display adapter to look like a physical GPU.
/// Walks HKLM\SYSTEM\CurrentControlSet\Enum\PCI, finds entries whose
/// FriendlyName/DeviceDescription match virtual adapter strings, and
/// rewrites them with a real GPU identity.
fn rename_display_adapter(vm_name: &str, report: &mut CleanupReport) {
    let (gpu_name, hw_id) = pick_gpu_profile();
    let script = format!(
        r#"
        $gpuName = '{gpu_name}'
        $hwId = '{hw_id}'
        $matches = @('Microsoft Hyper-V Video', 'Microsoft Basic Display Adapter', 'Virtual Display Adapter')
        $enumRoot = 'HKLM:\SYSTEM\CurrentControlSet\Enum'
        $found = 0

        Get-ChildItem $enumRoot -ErrorAction SilentlyContinue | ForEach-Object {{
            Get-ChildItem $_.PSPath -ErrorAction SilentlyContinue | ForEach-Object {{
                $devKey = $_.PSPath
                $desc = (Get-ItemProperty -Path $devKey -Name 'DeviceDesc' -ErrorAction SilentlyContinue).DeviceDesc
                $friendly = (Get-ItemProperty -Path $devKey -Name 'FriendlyName' -ErrorAction SilentlyContinue).FriendlyName
                $target = $desc
                if (-not $target) {{ $target = $friendly }}
                if ($target -and ($matches | Where-Object {{ $target -like "*$_*" }})) {{
                    Set-ItemProperty -Path $devKey -Name 'DeviceDesc' -Value $gpuName -Type String
                    Set-ItemProperty -Path $devKey -Name 'FriendlyName' -Value $gpuName -Type String
                    $hwKey = Join-Path $devKey 'Device Parameters'
                    if (Test-Path $hwKey) {{
                        Set-ItemProperty -Path $hwKey -Name 'MatchingDeviceId' -Value $hwId -Type String -ErrorAction SilentlyContinue
                    }}
                    $found++
                }}
            }}
        }}

        # Also patch the class-level display adapter entries
        $classRoot = 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{{4d36e968-e325-11ce-bfc1-08002be10318}}'
        if (Test-Path $classRoot) {{
            Get-ChildItem $classRoot -ErrorAction SilentlyContinue | Where-Object {{ $_.PSChildName -match '^\d{{4}}$' }} | ForEach-Object {{
                $desc = (Get-ItemProperty -Path $_.PSPath -Name 'DriverDesc' -ErrorAction SilentlyContinue).DriverDesc
                if ($desc -and ($matches | Where-Object {{ $desc -like "*$_*" }})) {{
                    Set-ItemProperty -Path $_.PSPath -Name 'DriverDesc' -Value $gpuName -Type String
                    Set-ItemProperty -Path $_.PSPath -Name 'ProviderName' -Value 'NVIDIA' -Type String -ErrorAction SilentlyContinue
                    $found++
                }}
            }}
        }}

        Write-Output "RENAMED:$found"
    "#
    );

    match run_in_guest(vm_name, &script) {
        Ok((stdout, _)) => {
            if stdout.contains("RENAMED:") {
                let count: u32 = stdout
                    .trim()
                    .lines()
                    .find_map(|l| l.strip_prefix("RENAMED:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if count > 0 {
                    report.display_adapter_renamed = Some(format!("{gpu_name} ({count} entries)"));
                } else {
                    report.errors.push("no virtual display adapter entries found to rename".into());
                }
            } else {
                report.errors.push("display adapter rename produced no parseable output".into());
            }
        }
        Err(e) => {
            report.errors.push(format!("display adapter rename failed: {e}"));
        }
    }
}

/// Full cleanup pipeline. Call this from your Tauri command layer.
///
/// # Example
/// ```ignore
/// let report = artifact::run_full_cleanup("MyVM")?;
/// println!("{report:#?}");
/// ```
pub fn run_full_cleanup(vm_name: &str) -> Result<CleanupReport, String> {
    let mut report = CleanupReport::new();

    // Registry first — cheap, fast, no restart needed.
    scrub_registry(vm_name, &mut report);

    // Kill Integration Services next.
    uninstall_integration_services(vm_name, &mut report);

    // Display adapter rename last since it walks the whole PCI enum tree.
    rename_display_adapter(vm_name, &mut report);

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_utf16() {
        // "dir" in UTF-16LE -> ZABpAHIA
        assert_eq!(base64::encode(&[0x64, 0x00, 0x69, 0x00, 0x72, 0x00]), "ZABpAHIA");
    }

    #[test]
    fn gpu_profiles_populated() {
        assert!(GPU_PROFILES.len() >= 3);
        for (name, id) in GPU_PROFILES {
            assert!(name.len() > 5, "GPU name too short: {name}");
            assert!(id.starts_with("PCI\\"), "Bad hardware ID: {id}");
        }
    }

    #[test]
    fn registry_keys_all_absolute() {
        for key in VM_REGISTRY_KEYS {
            assert!(key.starts_with("HKLM\\"), "Key not absolute: {key}");
        }
    }
}