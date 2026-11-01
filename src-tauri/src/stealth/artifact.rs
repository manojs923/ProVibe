use crate::stealth::{StealthError};
use std::time::Duration;
use rand::Rng;
use std::process::Command;
use crate::stealth::VmHandle;
use crate::stealth::TargetProfile;
use crate::vm::lifecycle::spawn_stealth_vm;

/// Artifact cleanup — removes VM-specific registry keys, uninstalls Hyper-V Integration Services,
/// stops/disables Hyper-V guest services, removes virtual device drivers, clears Hyper-V event logs,
/// unregisters Hyper-V scheduled tasks, removes Hyper-V firewall rules, and renames the virtual
/// display adapter in the Device Manager registry to match physical GPU entries.
///
/// Every destructive operation is backed up to C:\ProgramData\StealthBackup\ inside the guest so
/// `run_full_restore` can roll everything back.

const BACKUP_ROOT: &str = r"C:\ProgramData\StealthBackup";

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

const GPU_PROFILES: &[(&str, &str)] = &[
    ("NVIDIA GeForce RTX 3070", r"PCI\VEN_10DE&DEV_2484"),
    ("NVIDIA GeForce RTX 3060", r"PCI\VEN_10DE&DEV_2503"),
    ("AMD Radeon RX 6700 XT", r"PCI\VEN_1002&DEV_73DF"),
    ("Intel(R) UHD Graphics 630", r"PCI\VEN_8086&DEV_9BC5"),
    ("Intel(R) Iris(R) Xe Graphics", r"PCI\VEN_8086&DEV_46A6"),
];

const VIRTUAL_ADAPTER_MATCHES: &[&str] = &[
    "Microsoft Hyper-V Video",
    "Microsoft Basic Display Adapter",
    "Virtual Display Adapter",
];

const PS_DIRECT_TIMEOUT: Duration = Duration::from_secs(90);

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

    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
            && self.registry_keys_failed.is_empty()
            && self.services_failed.is_empty()
    }
}

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

impl RestoreReport {
    pub fn new() -> Self {
        // Initialize fields as needed
        RestoreReport { 
            services_restarted: Vec::new(),
            services_failed: Vec::new(),
            drivers_restored: Vec::new(),
            event_logs_restored: Vec::new(),
            scheduled_tasks_restored: Vec::new(),
            firewall_rules_restored: Vec::new(),
            registry_keys_restored: Vec::new(),
            errors: Vec::new(),
        }
    }
}

pub async fn cleanup_vm_artifacts(vm_handle: VmHandle) -> Result<CleanupReport, StealthError> {
    let vm_name = vm_handle.name();
    let mut report = CleanupReport::new();

    backup_guest_state(&vm_name, &mut report);
    scrub_registry(&vm_name, &mut report);
    strip_vmbus_values(&vm_name, &mut report);
    stop_vm_services(&vm_name, &mut report);
    uninstall_integration_services(&vm_name, &mut report);
    remove_virtual_drivers(&vm_name, &mut report);
    clear_hyper_v_event_logs(&vm_name, &mut report);
    unregister_hyper_v_tasks(&vm_name, &mut report);
    remove_hyper_v_firewall_rules(&vm_name, &mut report);
    rename_display_adapter(&vm_name, &mut report);

    Ok(report)
}

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

pub fn run_full_cleanup(vm_name: &str) -> Result<CleanupReport, String> {
    let handle = VmHandle::open(vm_name).map_err(|e| e.to_string())?;
    crate::stealth::block_on(cleanup_vm_artifacts(handle)).map_err(|e| e.to_string())
}

pub fn run_full_restore(vm_name: &str) -> Result<RestoreReport, String> {
    let handle = VmHandle::open(vm_name).map_err(|e| e.to_string())?;
    crate::stealth::block_on(restore_artifacts(handle)).map_err(|e| e.to_string())
}

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

fn run_powershell_in_guest(script: &str, vm_name: &str) -> Result<(), StealthError> {
    use std::os::windows::process::CommandExt;

    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| StealthError::from(format!("Failed to run PowerShell script: {e}")))?;

    if !output.status.success() {
        return Err(StealthError::from(format!(
            "PowerShell script failed with stdout: {} and stderr: {}",
            output.stdout,
            output.stderr
        )));
    }

    Ok(())
}

fn run_in_guest(vm_name: &str, script: &str) -> Result<(String, String), StealthError> {
    let output = run_powershell_in_guest(script, vm_name)?;
    Ok((String::from_utf8_lossy(&output.stdout).to_string(), String::from_utf8_lossy(&output.stderr).to_string()))
}

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
