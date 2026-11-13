// src-tauri/src/vm/lifecycle.rs
//
// VM lifecycle: boot wait, RDP readiness, spawn pipeline, checkpoint, destroy.
//
// All Hyper-V work goes through PowerShell with `-NonInteractive`, so a
// misbehaving cmdlet can never sit waiting on a prompt.

use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Command;

use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};

use crate::util::ps_literal;
use crate::vm::clone;
use crate::vm::network;

/// How many one-second polls wait_for_boot() allows.
const BOOT_RETRIES: u32 = 30;
/// How many half-second polls verify_rdp_ready() allows.
const RDP_RETRIES: u32 = 60;

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// A guest VM addressed by name.
///
/// Deliberately thin: it carries no Hyper-V object handles, so it stays
/// `Clone` and can cross task boundaries. `pid` is the console client's pid
/// when the handle came from a live session, and is absent otherwise.
#[derive(Debug, Clone)]
pub struct VmHandle {
    pub name: String,
    pub pid: Option<u32>,
}

impl VmHandle {
    /// Handle for a VM addressed by name alone.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pid: None,
        }
    }

    /// Attach the console client's pid.
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    /// The VM's name, as registered with Hyper-V.
    pub fn vm_name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// PowerShell
// ---------------------------------------------------------------------------

/// Silent PowerShell executor for lifecycle commands.
fn execute_ps(script: &str) -> Result<String, Error> {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(Error::other(format!("ps execution failed: {}", err)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Readiness
// ---------------------------------------------------------------------------

/// Blocks until the Hyper-V state confirms the VM is actually running.
pub async fn wait_for_boot(vm_name: &str) -> Result<(), Error> {
    let script = format!("(Get-VM -Name {}).State", ps_literal(vm_name));

    for _ in 0..BOOT_RETRIES {
        if let Ok(state) = execute_ps(&script) {
            if state.trim().eq_ignore_ascii_case("Running") {
                return Ok(());
            }
        }
        sleep(Duration::from_secs(1)).await;
    }

    Err(Error::new(
        ErrorKind::TimedOut,
        "vm failed to reach running state. is hyper-v completely fucked?",
    ))
}

/// Actively polls the loopback RDP port until the guest TCP stack is up.
pub async fn verify_rdp_ready(ip: &str) -> Result<(), Error> {
    let addr = format!("{}:3389", ip);

    // Spam SYNs until the RDP service responds. Usually 15-25s after boot.
    for _ in 0..RDP_RETRIES {
        if TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(500)).await;
    }

    Err(Error::new(
        ErrorKind::TimedOut,
        "rdp port 3389 never opened. check the vswitch routing.",
    ))
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Result handed back to the frontend.
#[derive(Debug, Clone)]
pub struct SpawnedVm {
    pub name: String,
    pub disk_path: PathBuf,
    pub rdp_address: String,
}

/// Full spawn pipeline: network isolation -> differencing disk -> vm creation
/// -> boot -> rdp poll.
pub async fn spawn_stealth_vm(
    vm_name: &str,
    base_image: &Path,
    vm_dir: &Path,
) -> Result<SpawnedVm, Error> {
    let net_config = network::get_network_config();
    let disk_dir = vm_dir.join("disks");

    // 1. Network: isolated private switch + loopback.
    network::setup_network(&net_config)
        .map_err(|e| Error::other(format!("network setup failed: {}", e)))?;

    // 2. Disk: differencing vhdx referencing the golden image.
    let disk = clone::create_differencing_disk(vm_name, base_image, &disk_dir)
        .map_err(|e| Error::other(format!("disk clone failed: {}", e)))?;

    // 3. Destroy a stale VM of the same name if a previous session crashed
    //    mid-teardown.
    let _ = execute_ps(&format!(
        "Stop-VM -Name {name} -TurnOff -Force -ErrorAction SilentlyContinue;\n\
         Remove-VM -Name {name} -Force -ErrorAction SilentlyContinue",
        name = ps_literal(vm_name)
    ));

    // 4. Create the VM: gen2, no dynamic memory (deterministic footprint),
    //    minimum specs since it only runs a proctoring app and RDP.
    let disk_str = disk.disk_path.to_string_lossy().to_string();
    execute_ps(&format!(
        "New-VM -Name {name} -Generation 2 -MemoryStartupBytes 2GB \
         -VHDPath {disk} -SwitchName {switch} -ErrorAction Stop;\n\
         Set-VM -Name {name} -ProcessorCount 2 -DynamicMemory:$false \
         -CheckpointType Disabled -AutomaticStartAction Nothing -AutomaticStopAction TurnOff;\n\
         Set-VMProcessor -VMName {name} -ExposeVirtualizationExtensions $false;\n\
         Set-VMKeyProtector -VMName {name} -NewLocalKeyProtector -ErrorAction SilentlyContinue",
        name = ps_literal(vm_name),
        disk = ps_literal(&disk_str),
        switch = ps_literal(&net_config.switch_name)
    ))?;

    // 5. Materialise the memory object so the first guest read is cheap.
    let _ = execute_ps(&format!(
        "Get-VM -Name {} | Get-VMMemory | Out-Null",
        ps_literal(vm_name)
    ));

    // 6. Boot.
    execute_ps(&format!(
        "Start-VM -Name {} -ErrorAction Stop",
        ps_literal(vm_name)
    ))?;

    // 7. Wait for running state, then RDP readiness on loopback.
    wait_for_boot(vm_name).await?;
    verify_rdp_ready(&net_config.loopback_ip).await?;

    // 8. Final leak check before handing the session to the user.
    let leak =
        network::detect_route_leak(&net_config).map_err(|e| Error::other(e))?;
    if leak {
        network::remediate_leak(&net_config).map_err(|e| Error::other(e))?;
    }

    Ok(SpawnedVm {
        name: vm_name.to_string(),
        disk_path: disk.disk_path,
        rdp_address: net_config.loopback_ip,
    })
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

/// Creates a Hyper-V checkpoint for instant redeployment.
pub fn create_checkpoint(vm_name: &str, checkpoint_name: &str) -> Result<(), Error> {
    execute_ps(&format!(
        "Checkpoint-VM -Name {name} -SnapshotName {snap} -ErrorAction Stop",
        name = ps_literal(vm_name),
        snap = ps_literal(checkpoint_name)
    ))?;
    Ok(())
}

/// Obliterates current state and instantly reverts to the clean snapshot.
pub fn restore_checkpoint(vm_name: &str, checkpoint_name: &str) -> Result<(), Error> {
    execute_ps(&format!(
        "Restore-VMSnapshot -Name {snap} -VMName {name} -Confirm:$false -ErrorAction Stop;\n\
         Start-VM -Name {name};",
        name = ps_literal(vm_name),
        snap = ps_literal(checkpoint_name)
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Destroy
// ---------------------------------------------------------------------------

/// Scorched earth. Forcefully kills the process, deletes the VM, destroys the
/// differencing disk (all session writes), and tears down network state. The
/// base image survives untouched.
pub fn destroy_vm(vm_name: &str, disk_path: Option<&Path>) -> Result<(), Error> {
    execute_ps(&format!(
        "Stop-VM -Name {name} -TurnOff -Force -ErrorAction SilentlyContinue;\n\
         Remove-VM -Name {name} -Force -ErrorAction SilentlyContinue;",
        name = ps_literal(vm_name)
    ))?;

    // Nuke the delta disk — this is where all session activity (proctoring
    // app logs, browser history, exam artifacts) lives. Gone means gone.
    if let Some(path) = disk_path {
        let _ = clone::destroy_differencing_disk(path);
    }

    // Tear down network residue.
    let _ = network::destroy_network(&network::get_network_config());

    Ok(())
}