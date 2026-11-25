use std::path::PathBuf;

use crate::hidden_command;

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub switch_name: String,
    pub adapter_name: String,
    pub loopback_ip: String,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            switch_name: "HVSandboxSwitch".to_string(),
            adapter_name: "HVSandboxLoopback".to_string(),
            loopback_ip: "127.0.0.2".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkResult {
    pub switch_created: bool,
    pub loopback_configured: bool,
    pub has_external_route: bool,
}

fn run_powershell(command: &str) -> Result<String, String> {
    let output = hidden_command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .output()
        .map_err(|e| format!("Failed to spawn PowerShell: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

/// Checks if the isolated switch already exists from a previous session.
/// Private switches persist across reboots, so we reuse instead of erroring.
pub fn switch_exists(config: &NetworkConfig) -> Result<bool, String> {
    let result = run_powershell(&format!(
        "Get-VMSwitch -Name '{}' -ErrorAction SilentlyContinue | Measure-Object | Select-Object -ExpandProperty Count",
        config.switch_name
    ))?;

    Ok(result.trim() == "1")
}

/// Creates the internal/private virtual switch.
/// PRIVATE type: no binding to any physical NIC, no host vNIC created.
/// The VM can talk to the host via the loopback adapter only if we explicitly allow it.
pub fn create_isolated_switch(config: &NetworkConfig) -> Result<bool, String> {
    if switch_exists(config)? {
        return Ok(false); // Already exists, nothing to do.
    }

    run_powershell(&format!(
        "New-VMSwitch -Name '{}' -SwitchType Private -Notes 'Sandbox isolated switch, no external binding'",
        config.switch_name
    ))?;

    Ok(true)
}

/// Verifies the switch has no external bindings by checking its type.
/// If some other process flipped it to External, we nuke and recreate.
/// Some lockdown browser installers try to modify network config to phone home.
pub fn verify_isolation(config: &NetworkConfig) -> Result<(), String> {
    let switch_type = run_powershell(&format!(
        "Get-VMSwitch -Name '{}' | Select-Object -ExpandProperty SwitchType",
        config.switch_name
    ))?;

    let trimmed = switch_type.trim();
    if trimmed != "Private" {
        // Switch was tampered with or misconfigured. Destroy and rebuild.
        run_powershell(&format!(
            "Remove-VMSwitch -Name '{}' -Force",
            config.switch_name
        ))?;
        create_isolated_switch(config)?;
        return Ok(());
    }

    // Belt and suspenders: check if any external adapters snuck in.
    let bindings = run_powershell(&format!(
        "(Get-VMSwitch -Name '{}').NetAdapterInterfaceDescription | Measure-Object | Select-Object -ExpandProperty Count",
        config.switch_name
    ))?;

    if bindings.trim() != "0" {
        return Err(format!(
            "Switch '{}' has unexpected external NIC bindings. Quarantine failed.",
            config.switch_name
        ));
    }

    Ok(())
}

/// Configures the host loopback adapter at 127.0.0.2.
/// This is the address the VM's forwarded RDP connection resolves to.
/// Loopback is never routable externally, so the VM's network stack
/// is invisible to anything outside this machine even if the switch leaks.
pub fn configure_loopback_adapter(config: &NetworkConfig) -> Result<bool, String> {
    let exists = run_powershell(&format!(
        "Get-NetAdapter -Name '{}' -ErrorAction SilentlyContinue | Measure-Object | Select-Object -ExpandProperty Count",
        config.adapter_name
    ))?;

    if exists.trim() == "1" {
        // Adapter exists, verify IP is still bound.
        let bound = run_powershell(&format!(
            "Get-NetIPAddress -IPAddress '{}' -ErrorAction SilentlyContinue | Measure-Object | Select-Object -ExpandProperty Count",
            config.loopback_ip
        ))?;
        if bound.trim() == "1" {
            return Ok(false); // Already good.
        }
    } else {
        // Install the KM-TEST loopback adapter via pnputil (silent, no driver signing popup).
        let status = hidden_command("pnputil")
            .args(["/add-driver", "C:\\Windows\\INF\\netloop.inf", "/install"])
            .output()
            .map_err(|e| format!("Failed to spawn pnputil: {}", e))?;

        if !status.status.success() {
            return Err("Failed to install loopback adapter driver. Is this Windows 11 with admin rights?".to_string());
        }

        // The adapter shows up with a generic name, rename it to ours.
        run_powershell(
            "Get-NetAdapter -InterfaceDescription 'Microsoft KM-TEST Loopback Adapter' | Rename-NetAdapter -NewName 'HVSandboxLoopback'",
        )?;
    }

    // Bind 127.0.0.2 with a /32 mask so it doesn't announce itself on any subnet.
    run_powershell(&format!(
        "New-NetIPAddress -InterfaceAlias '{}' -IPAddress '{}' -PrefixLength 32 -ErrorAction SilentlyContinue",
        config.adapter_name, config.loopback_ip
    ))?;

    Ok(true)
}

/// Full network teardown. Removes the loopback IP binding and destroys the switch.
/// After this, netstat shows nothing unusual. The loopback adapter itself
/// can stay installed (it's a stock Windows driver, not suspicious),
/// just unbound from any address.
pub fn destroy_network(config: &NetworkConfig) -> Result<(), String> {
    // Unbind the loopback IP.
    run_powershell(&format!(
        "Remove-NetIPAddress -IPAddress '{}' -ErrorAction SilentlyContinue -Confirm:$false",
        config.loopback_ip
    ))?;

    // Kill the switch.
    if switch_exists(config)? {
        run_powershell(&format!(
            "Remove-VMSwitch -Name '{}' -Force",
            config.switch_name
        ))?;
    }

    Ok(())
}

/// Detects if the VM somehow acquired an external route.
/// Call this periodically as a watchdog — if a lockdown browser inside the VM
/// tried to bridge the private switch or enable ICS, this catches it.
pub fn detect_route_leak(config: &NetworkConfig) -> Result<bool, String> {
    // Any enabled ICS connections would let the VM piggyback out.
    let ics = run_powershell(
        "Get-NetConnectionSharing | Where-Object {$_.SharingEnabled -eq $true} | Measure-Object | Select-Object -ExpandProperty Count"
    )?;

    if ics.trim() != "0" {
        return Ok(true); // Leak detected.
    }

    // Check no external bindings on our switch.
    verify_isolation(config)?;

    Ok(false)
}

/// Remediation if a leak is detected: disable ICS globally and rebuild the switch.
/// ICS being on is itself suspicious to monitoring software, so we kill it entirely.
pub fn remediate_leak(config: &NetworkConfig) -> Result<(), String> {
    run_powershell("Set-Service -Name SharedAccess -StartupType Disabled; Stop-Service -Name SharedAccess -Force -ErrorAction SilentlyContinue")?;

    if switch_exists(config)? {
        run_powershell(&format!(
            "Remove-VMSwitch -Name '{}' -Force",
            config.switch_name
        ))?;
    }
    create_isolated_switch(config)?;

    Ok(())
}

/// Full setup pipeline. Call from spawn_stealth_vm before VM boot.
pub fn setup_network(config: &NetworkConfig) -> Result<NetworkResult, String> {
    let switch_created = create_isolated_switch(config)?;
    verify_isolation(config)?;
    let loopback_configured = configure_loopback_adapter(config)?;

    Ok(NetworkResult {
        switch_created,
        loopback_configured,
        has_external_route: false, // By construction, private switch has none.
    })
}

/// Returns the config used for this session. Useful for the lifecycle module
/// to reference the switch when creating the VM's network adapter.
pub fn get_network_config() -> NetworkConfig {
    NetworkConfig::default()
}