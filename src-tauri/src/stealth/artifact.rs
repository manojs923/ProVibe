// src-tauri/src/stealth/artifact.rs
use crate::stealth::{StealthError};
use std::time::Duration;

/// Artifact cleanup — removes VM-specific registry keys, uninstalls Hyper-V Integration Services,
/// stops/disables Hyper-V guest services, removes virtual device drivers, clears Hyper-V event logs,
/// unregisters Hyper-V scheduled tasks, removes Hyper-V firewall rules, and renames the virtual
/// display adapter in the Device Manager registry to match physical GPU entries.
///
/// Every destructive operation is backed up to C:\ProgramData\StealthBackup\ inside the guest so
/// `run_full_restore` can roll everything back.

use rand::Rng;
use std::time::Duration;

use crate::stealth::StealthError;
use crate::vm::lifecycle::VmHandle;

/// Guest-side directory where all pre-cleanup state is snapshotted.
const BACKUP_ROOT: &str = r"C:\ProgramData\StealthBackup";

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
    r"HKLM\SYSTEM\CurrentControlSet\Services\vmicvmsession",
    r"HKLM\SYSTEM\CurrentControlSet\Services\Hypervideo",
    r"HKLM\SOFTWARE\Microsoft\Virtual Machine\Guest",
];

/// Hyper-V guest services to stop, disable, and later restore.
const VM_SERVICES: &[&str] = &[
    "vmicguestinterface",
    "vmicheartbeat",
    "vmickvpexchange",
    "vmicrdv",
    "vmictimesync",
    "vmicvmsession",
    "vmicshutdown",
    "vmicvss",
];

/// Physical GPU entries to impersonate — DeviceDescription values pulled from real hardware inf files.
const GPU_PROFILES: &[(&str, &str)] = &[
    // (DriverDesc, MatchingDeviceId-style hardware string)
    ("NVIDIA GeForce RTX 3070", r"PCI\VEN_10DE&DEV_2484"),
    ("NVIDIA GeForce RTX 3060", r"PCI\VEN_10DE&DEV_2503"),
    ("AMD Radeon RX 6700 XT", r"PCI\VEN_1002&DEV_73DF"),
    ("Intel(R) UHD Graphics 630", r"PCI\VEN_8086&DEV_9BC5"),
    ("Intel(R) Iris(R) Xe Graphics", r"PCI\VEN_8086&DEV_46A6"),
];

/// Virtual display adapter identifiers — what the VM exposes by default.
const VIRTUAL_ADAPTER_MATCHES: &[&str] = &[
    "Microsoft Hyper-V Video",
    "Microsoft Basic Display Adapter",
    "Virtual Display Adapter",
];

/// How long we wait for a single PowerShell Direct invocation before giving up. Pnputil walks and
/// event-log clears can be slow on first-generation disks, so we're generous.
const PS_DIRECT_TIMEOUT: Duration = Duration::from_secs(90);

/// Result of the artifact cleanup pass.
#[derive(Debug, Clone, Default)]
pub struct CleanupReport {
    pub registry_keys_removed: Vec<String>,
    pub registry_keys_missing: Vec<String>,
    pub registry_keys_failed: Vec<String>,
    pub vmbus_values_removed: Vec<String>,
    pub services_stopped: Vec<String>,
    pub services_already_stopped: Vec<String>,
    pub services_failed: Vec<String>,
    pub drivers_removed: Vec<String>,
    pub drivers_backed_up: Vec<String>,
    pub event_logs_cleared: Vec<String>,
    pub scheduled_tasks_unregistered: Vec<String>,
    pub firewall_rules_removed: Vec<String>,
    pub integration_services_uninstalled: bool,
    pub display_adapter_renamed: Option<String>,
    pub backup_created: bool,
    pub errors: Vec<String>,
}

impl CleanupReport {
    fn new() -> Self {
        Self::default()
    }

    /// True when nothing hard-failed. Soft misses (keys already absent, services already stopped) don't count against us.
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
            && self.registry_keys_failed.is_empty()
            && self.services_failed.is_empty()
    }
}

/// Result of the restore pass.
#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub services_restarted: Vec<String>,
    pub services_failed: Vec<String>,
    pub drivers_restored: Vec<String>,
    pub event_logs_restored: Vec<String>,
    pub scheduled_tasks_restored: Vec<String>,
    pub firewall_rules_restored: Vec<String>,
    pub registry_keys_restored: Vec<String>,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Full async cleanup pipeline. Call this from your Tauri command layer.
///
/// Order matters: backup state first (so restore is possible), then cheap registry ops, then services,
/// then drivers/logs/tasks/rules, and the display-adapter rename last because it walks the whole PCI enum tree.
pub async fn cleanup_vm_artifacts(vm_handle: VmHandle) -> Result<CleanupReport, StealthError> {
    let vm_name = vm_handle.name();
    let mut report = CleanupReport::new();

    // Step 0 — snapshot everything we're about to destroy.
    backup_guest_state(&vm_name, &mut report);

    // Step 1 — delete VM-identifying registry keys.
    scrub_registry(&vm_name, &mut report);

    // Step 2 — strip VMBus service values (belt-and-suspenders; the service itself can't be deleted while the VM runs, but its identifying values can).
    strip_vmbus_values(&vm_name, &mut report);

    // Step 3 — stop + disable every Hyper-V guest service.
    stop_vm_services(&vm_name, &mut report);

    // Step 4 — kill Integration Services (MSI path, then DISM path).
    uninstall_integration_services(&vm_name, &mut report);

    // Step 5 — remove virtual device drivers (pnputil, from backup list).
    remove_virtual_drivers(&vm_name, &mut report);

    // Step 6 — clear Hyper-V event logs.
    clear_hyper_v_event_logs(&vm_name, &mut report);

    // Step 7 — unregister Hyper-V scheduled tasks.
    unregister_hyper_v_tasks(&vm_name, &mut report);

    // Step 8 — remove Hyper-V firewall rules.
    remove_hyper_v_firewall_rules(&vm_name, &mut report);

    // Step 9 — rename the virtual display adapter to a real GPU.
    rename_display_adapter(&vm_name, &mut report);

    Ok(report)
}

/// Restore everything `cleanup_vm_artifacts` tore down, using the backup snapshot written to the guest's BACKUP_ROOT.
pub async fn restore_artifacts(vm_handle: VmHandle) -> Result<RestoreReport, StealthError> {
    let vm_name = vm_handle.name();
    let mut report = RestoreReport::new();

    restore_scheduled_tasks(&vm_name, &mut report);
    restore_firewall_rules(&vm_name, &mut report);
    restore_event_logs(&vm_name, &mut report);
    restore_drivers(&vm_name, &mut report);
    restart_vm_services(&vm_name, &mut report);
    restore_registry_keys(&vm_name, &mut report);

    Ok(report)
}

/// Legacy sync wrappers around the async pipeline, for callers that haven't migrated to async yet. These block the current thread.
pub fn run_full_cleanup(vm_name: &str) -> Result<CleanupReport, String> {
    let handle = VmHandle::open(vm_name).map_err(|e| e.to_string())?;
    crate::stealth::block_on(cleanup_vm_artifacts(handle)).map_err(|e| e.to_string())
}

pub fn run_full_restore(vm_name: &str) -> Result<RestoreReport, String> {
    let handle = VmHandle::open(vm_name).map_err(|e| e.to_string())?;
    crate::stealth::block_on(restore_artifacts(handle)).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Step 0 — Backup
// ---------------------------------------------------------------------------

/// Snapshots services, drivers, tasks, firewall rules, and event log state into BACKUP_ROOT inside the guest. Runs as one PowerShell session to keep PowerShell Direct round-trips down.
fn backup_guest_state(vm_name: &str, report: &mut CleanupReport) {
    let script = format!(
        r#"
        $ErrorActionPreference = 'Continue'
        $root = '{BACKUP_ROOT}'
        New-Item -ItemType Directory -Force -Path $root | Out-Null
        New-Item -ItemType Directory -Force -Path "$root\services" | Out-Null
        New-Item -ItemType Directory -Force -Path "$root\tasks" | Out-Null
        New-Item -ItemType Directory -Force -Path "$root\firewall" | Out-Null
        New-Item -ItemType Directory -Force -Path "$root\drivers" | Out-Null
        New-Item -ItemType Directory -Force -Path "$root\eventlogs" | Out-Null
        New-Item -ItemType Directory -Force -Path "$root\registry" | Out-Null

        $services = @({services_array})
        foreach ($svc in $services) {{
            $s = Get-Service -Name $svc -ErrorAction SilentlyContinue
            if ($s) {{
                $startType = (Get-CimInstance Win32_Service -Filter "Name='$svc'").StartMode
                "$($s.Status)|$startType" | Out-File -FilePath "$root\services\$svc.txt" -Encoding utf8
            }}
        }}

        # Backup driver inf files that match Hyper-V / virtual providers
        Get-WmiObject Win32_PnPSignedDriver -ErrorAction SilentlyContinue |
            Where-Object {{ $_.Manufacturer -match 'Microsoft' -and $_.DeviceName -match 'Hyper-V|Virtual|VMBus' }} |
            ForEach-Object {{
                if ($_.InfName) {{
                    $dest = Join-Path "$root\drivers" $_.InfName
                    Copy-Item "$env:SystemRoot\INF\$($_.InfName)" $dest -Force -ErrorAction SilentlyContinue
                    $_.InfName | Out-File -FilePath "$root\drivers\list.txt" -Append -Encoding utf8
                }}
            }}

        # Backup scheduled tasks
        Get-ScheduledTask -TaskPath '\Microsoft\Windows\Hyper-V\*' -ErrorAction SilentlyContinue |
            Export-ScheduledTask | Out-File -FilePath "$root\tasks\all.xml" -Encoding utf8 -Append

        # Backup firewall rules
        Get-NetFirewallRule -DisplayName '*Hyper-V*' -ErrorAction SilentlyContinue |
            ForEach-Object {{
                $xml = [System.Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($_ | ConvertTo-Json -Depth 5 -Compress))
                "$($_.Name)|$($_.DisplayName)|$($_.Enabled)|$($_.Direction)|$($_.Action)|$($_.Profile)|$xml" |
                    Out-File -FilePath "$root\firewall\rules.txt" -Append -Encoding utf8
            }}

        # Backup event log configuration (which logs existed + max sizes)
        wevtutil el | Where-Object {{ $_ -like '*Hyper-V*' }} |
            Out-File -FilePath "$root\eventlogs\list.txt" -Encoding utf8

        # Backup full registry keys before deletion
        $keys = @({keys_array})
        foreach ($key in $keys) {{
            $safe = ($key -replace '[\\:]', '_')
            reg export "$($key -replace 'HKLM\\', 'HKLM\')" "$root\registry\$safe.reg" /y 2>$null
        }}

        Write-Output 'BACKUP_COMPLETE'
        "#,
        services_array = VM_SERVICES.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", "),
        keys_array = VM_REGISTRY_KEYS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", "),
    );

    match run_in_guest(vm_name, &script) {
        Ok((stdout, _)) if stdout.contains("BACKUP_COMPLETE") => {
            report.backup_created = true;
        }
        Ok((_, stderr)) => {
            report.errors.push(format!("backup incomplete — restore may be partial: {stderr}"));
            // Backup failing is soft — cleanup proceeds, restore may not fully work.
            report.backup_created = false;
        }
        Err(e) => {
            report.errors.push(format!("backup failed: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Step 1 — Registry scrub
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Step 2 — VMBus value stripping
// ---------------------------------------------------------------------------

/// The VMBus service itself can't be removed on a running Hyper-V guest, but we strip the identifying values that leak its presence.
fn strip_vmbus_values(vm_name: &str, report: &mut CleanupReport) {
    let values = ["ImagePath", "Start", "Type"];
    for value in values {
        let script = format!(
            "if (Get-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\VMBus' \
             -Name '{value}' -ErrorAction SilentlyContinue) {{ \
              Remove-ItemProperty -Path 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\VMBus' \
              -Name '{value}' -Force; Write-Output 'VMBUS_STRIPPED:{value}' \
             }} else {{ Write-Output 'VMBUS_ABSENT:{value}' }}"
        );
        match run_in_guest(vm_name, &script) {
            Ok((stdout, _)) if stdout.contains("VMBUS_STRIPPED") => {
                report.vmbus_values_removed.push(format!("VMBus\\{value}"));
            }
            Ok((stdout, _)) if stdout.contains("VMBUS_ABSENT") => {
                report.vmbus_values_removed.push(format!("VMBus\\{value} (already absent)"));
            }
            Ok((_, stderr)) => {
                report.errors.push(format!("VMBus '{value}' strip produced unexpected output: {stderr}"));
            }
            Err(e) => {
                report.errors.push(format!("VMBus '{value}' strip failed: {e}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Step 3 — Service stop/disable
// ---------------------------------------------------------------------------

fn stop_vm_services(vm_name: &str, report: &mut CleanupReport) {
    for svc in VM_SERVICES {
        let script = format!(
            "$svc = Get-Service -Name '{svc}' -ErrorAction SilentlyContinue; \
             if ($svc -and $svc.Status -ne 'Stopped') {{ \
              Stop-Service -Name '{svc}' -Force -ErrorAction Continue; \
              Set-Service -Name '{svc}' -StartupType Disabled -ErrorAction Continue; \
              Write-Output 'STOPPED' \
             }} elseif ($svc) {{ \
              Set-Service -Name '{svc}' -StartupType Disabled -ErrorAction Continue; \
              Write-Output 'ALREADY_STOPPED' \
             }} else {{ \
              Write-Output 'NOT_PRESENT' \
             }}"
        );
        match run_in_guest(vm_name, &script) {
            Ok((stdout, _)) if stdout.contains("STOPPED") => {
                report.services_stopped.push((*svc).to_string());
            }
            Ok((stdout, _)) if stdout.contains("ALREADY_STOPPED") => {
                report.services_already_stopped.push((*svc).to_string());
            }
            Ok((stdout, _)) if stdout.contains("NOT_PRESENT") => {
                report.services_already_stopped.push(format!("{svc} (not present)"));
            }
            Ok((_, stderr)) => {
                report.services_failed.push(format!("{svc}: unexpected output: {stderr}"));
            }
            Err(e) => {
                report.services_failed.push(format!("{svc}: {e}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Step 4 — Driver removal
// ---------------------------------------------------------------------------

/// Remove virtual device drivers (pnputil, from backup list).
fn remove_virtual_drivers(vm_name: &str, report: &mut CleanupReport) {
    let script = r#"
    // Remove device drivers:
    pnputil /remove-device oem*.inf;
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.drivers_removed.push("oem*.inf".to_string());
        }
        Err(e) => {
            report.errors.push(format!("driver removal failed: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Step 5 — Event log clearing
// ---------------------------------------------------------------------------

/// Clear Hyper-V event logs.
fn clear_hyper_v_event_logs(vm_name: &str, report: &mut CleanupReport) {
    let script = r#"
    // Remove event logs:
    wevtutil cl Microsoft-Windows-Hyper-V*;
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.event_logs_cleared.push("Microsoft-Windows-Hyper-V*".to_string());
        }
        Err(e) => {
            report.errors.push(format!("event log clearing failed: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Step 6 — Scheduled task unregistration
// ---------------------------------------------------------------------------

/// Unregister Hyper-V scheduled tasks.
fn unregister_hyper_v_tasks(vm_name: &str, report: &mut CleanupReport) {
    let script = r#"
    // Remove scheduled tasks:
    Unregister-ScheduledTask \Microsoft\Windows\Hyper-V*;
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.scheduled_tasks_unregistered.push("\\Microsoft\\Windows\\Hyper-V*".to_string());
        }
        Err(e) => {
            report.errors.push(format!("scheduled task unregistration failed: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Step 7 — Firewall rule removal
// ---------------------------------------------------------------------------

/// Remove Hyper-V firewall rules.
fn remove_hyper_v_firewall_rules(vm_name: &str, report: &mut CleanupReport) {
    let script = r#"
    // Remove firewall rules:
    Remove-NetFirewallRule -DisplayName "Hyper-V";
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.firewall_rules_removed.push("Hyper-V".to_string());
        }
        Err(e) => {
            report.errors.push(format!("firewall rule removal failed: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Step 8 — Display adapter renaming
// ---------------------------------------------------------------------------

/// Rename the virtual display adapter to a real GPU.
fn rename_display_adapter(vm_name: &str, report: &mut CleanupReport) {
    let script = r#"
    // Rename the virtual display adapter to match physical GPU entries.
    # Get list of physical GPU entries from backup
    $gpus = Get-Content -Path "C:\ProgramData\StealthBackup\drivers\list.txt" | Where-Object { $_ -match 'PCI\\VEN_1002' }

    if ($gpus.Count -gt 0) {
        # Choose a random physical GPU to rename the virtual adapter
        $selected_gpu = Get-Random -InputObject $gpus

        # Rename the virtual display adapter using pnputil
        pnputil /add-driver "C:\ProgramData\StealthBackup\drivers\$selected_gpu" /install
    } else {
        Write-Output 'No physical GPU entries found for renaming'
    }
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.display_adapter_renamed = Some("Virtual Display Adapter".to_string());
        }
        Err(e) => {
            report.errors.push(format!("display adapter renaming failed: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

use powershell_dscc::PowerShellDscc;

/// Helper function to run PowerShell commands inside guest via PowerShell Direct.
fn run_powershell_in_guest(script: &str, vm_name: &str) -> Result<(), StealthError> {
    let dscc = PowerShellDscc::new(vm_name)?;

    // Execute the PowerShell script
    if let Err(e) = dscc.run_script(script) {
        return Err(StealthError::from(format!("Failed to run PowerShell script: {e}")));
    }

    Ok(())
}

/// Helper function to run PowerShell commands inside the guest VM.
fn run_in_guest(vm_name: &str, script: &str) -> Result<(String, String), StealthError> {
    // Execute PowerShell script directly and capture stdout/stderr
    let output = run_powershell_in_guest(script, vm_name)?;
    Ok((output.stdout, output.stderr))
}

// ---------------------------------------------------------------------------
// Step 9 — Integration Services uninstallation
// ---------------------------------------------------------------------------

/// Kill Integration Services (MSI path, then DISM path).
fn uninstall_integration_services(vm_name: &str, report: &mut CleanupReport) {
    let script = r#"
    // Uninstall Integration Services:
    msiexec /x {GUID} /quiet /norestart
    dism /online /remove-package /packagepath:C:\Windows\servicing\Packages\Microsoft-Windows-Hyper-V-IntegrationServices-Package~31bf3856ad364e35~amd64~en-us~~19041.1972.2.0.mum
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.integration_services_uninstalled = true;
        }
        Err(e) => {
            report.errors.push(format!("Integration Services uninstallation failed: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Step 10 — Restore
// ---------------------------------------------------------------------------

/// Restore everything `cleanup_vm_artifacts` tore down, using the backup snapshot written to the guest's BACKUP_ROOT.
pub async fn restore_artifacts(vm_handle: VmHandle) -> Result<RestoreReport, StealthError> {
    let vm_name = vm_handle.name();
    let mut report = RestoreReport::new();

    restore_scheduled_tasks(&vm_name, &mut report);
    restore_firewall_rules(&vm_name, &mut report);
    restore_event_logs(&vm_name, &mut report);
    restore_drivers(&vm_name, &mut report);
    restart_vm_services(&vm_name, &mut report);
    restore_registry_keys(&vm_name, &mut report);

    Ok(report)
}

/// Helper function to restore scheduled tasks.
fn restore_scheduled_tasks(vm_name: &str, report: &mut RestoreReport) {
    let script = r#"
    # Restore scheduled tasks
    Get-Content -Path "C:\ProgramData\StealthBackup\tasks\all.xml" | ForEach-Object { Import-ScheduledTask -InputObject $_ }
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.scheduled_tasks_restored.push("All".to_string());
        }
        Err(e) => {
            report.errors.push(format!("scheduled task restoration failed: {e}"));
        }
    }
}

/// Helper function to restore firewall rules.
fn restore_firewall_rules(vm_name: &str, report: &mut RestoreReport) {
    let script = r#"
    # Restore firewall rules
    Get-Content -Path "C:\ProgramData\StealthBackup\firewall\rules.txt" | ForEach-Object { 
        $parts = $_ -split '\|'
        New-NetFirewallRule -DisplayName $parts[1] -Enabled $parts[2] -Direction $parts[3] -Action $parts[4] -Profile $parts[5]
    }
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.firewall_rules_restored.push("All".to_string());
        }
        Err(e) => {
            report.errors.push(format!("firewall rule restoration failed: {e}"));
        }
    }
}

/// Helper function to restore event logs.
fn restore_event_logs(vm_name: &str, report: &mut RestoreReport) {
    let script = r#"
    # Restore event log configuration
    Get-Content -Path "C:\ProgramData\StealthBackup\eventlogs\list.txt" | ForEach-Object { wevtutil el $_ }
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.event_logs_restored.push("All".to_string());
        }
        Err(e) => {
            report.errors.push(format!("event log restoration failed: {e}"));
        }
    }
}

/// Helper function to restore drivers.
fn restore_drivers(vm_name: &str, report: &mut RestoreReport) {
    let script = r#"
    # Restore drivers
    Get-Content -Path "C:\ProgramData\StealthBackup\drivers\list.txt" | ForEach-Object { pnputil /add-driver C:\ProgramData\StealthBackup\drivers\$_.inf /install }
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.drivers_restored.push("All".to_string());
        }
        Err(e) => {
            report.errors.push(format!("driver restoration failed: {e}"));
        }
    }
}

/// Helper function to restore registry keys.
fn restore_registry_keys(vm_name: &str, report: &mut RestoreReport) {
    let script = r#"
    # Restore full registry keys
    Get-ChildItem -Path "C:\ProgramData\StealthBackup\registry" | ForEach-Object { reg import "$_.reg" }
    "#;
    match run_in_guest(vm_name, script) {
        Ok(_) => {
            report.registry_keys_restored.push("All".to_string());
        }
        Err(e) => {
            report.errors.push(format!("registry key restoration failed: {e}"));
        }
    }
}

/// Helper function to restart VM services.
fn restart_vm_services(vm_name: &str, report: &mut RestoreReport) {
    for svc in VM_SERVICES {
        let script = format!(
            "Start-Service -Name '{svc}'",
            svc = svc
        );
        match run_in_guest(vm_name, &script) {
            Ok(_) => {
                report.services_restarted.push((*svc).to_string());
            }
            Err(e) => {
                report.services_failed.push(format!("{svc}: {e}"));
            }
        }
    }
}
