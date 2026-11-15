// src-tauri/src/vm/lifecycle.rs
//
// VM lifecycle: boot wait, RDP readiness, spawn pipeline, checkpoint, destroy.
//
// All Hyper-V work goes through PowerShell with `-NonInteractive`, so a
// misbehaving cmdlet can never sit waiting on a prompt. Every script runs on
// the blocking pool — `Command::output()` would otherwise stall the reactor.

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
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pid: None,
        }
    }

    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    pub fn vm_name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// PowerShell
// ---------------------------------------------------------------------------

/// Silent PowerShell executor. Blocking — call via `execute_ps_async` from
/// async contexts.
fn execute_ps(script: &str) -> Result<String, Error> {
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
        return Err(Error::other(format!("ps execution failed: {}", err)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn execute_ps_async(script: String) -> Result<String, Error> {
    tokio::task::spawn_blocking(move || execute_ps(&script))
        .await
        .map_err(|e| Error::other(format!("ps task panicked: {}", e)))?
}

// ---------------------------------------------------------------------------
// Readiness
// ---------------------------------------------------------------------------

/// Blocks until the Hyper-V state confirms the VM is actually running.
///
/// Takes the VM *name*, not the loopback address.
pub async fn wait_for_boot(vm_name: &str) -> Result<(), Error> {
    let script = format!("(Get-VM -Name {}).State", ps_literal(vm_name));

    for _ in 0..BOOT_RETRIES {
        if let Ok(state) = execute_ps_async(script.clone()).await {
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

/// Full spawn pipeline: cleanup -> network isolation -> differencing disk ->
/// vm creation -> boot -> rdp poll.
pub async fn spawn_stealth_vm(
    vm_name: &str,
    base_image: &Path,
    vm_dir: &Path,
) -> Result<SpawnedVm, Error> {
    let net_config = network::get_network_config();
    let disk_dir = vm_dir.join("disks");

    // 1. Cleanup FIRST. The differencing disk lives at a deterministic path,
    //    so a crashed prior session would make New-VHD collide below. Both the
    //    VM record and the stale delta have to go before we recreate either.
    let stale_disk = disk_dir.join(format!("{}.vhdx", vm_name));
    let _ = execute_ps_async(format!(
        "Stop-VM -Name {name} -TurnOff -Force -ErrorAction SilentlyContinue;\n\
         Remove-VM -Name {name} -Force -ErrorAction SilentlyContinue",
        name = ps_literal(vm_name)
    ))
    .await;
    if stale_disk.exists() {
        let _ = clone::destroy_differencing_disk(&stale_disk);
    }

    // 2. Network: isolated private switch + loopback.
    let net = net_config.clone();
    tokio::task::spawn_blocking(move || network::setup_network(&net))
        .await
        .map_err(|e| Error::other(format!("network task panicked: {}", e)))?
        .map_err(|e| Error::other(format!("network setup failed: {}", e)))?;

    // 3. Disk: differencing vhdx referencing the golden image.
    let disk = clone::create_differencing_disk(vm_name, base_image, &disk_dir)
        .map_err(|e| Error::other(format!("disk clone failed: {}", e)))?;

    // 4. Create the VM: gen2, fixed memory (deterministic footprint), minimum
    //    specs since it only runs a proctoring app and RDP.
    let disk_str = disk.disk_path.to_string_lossy().to_string();
    execute_ps_async(format!(
        "New-VM -Name {name} -Generation 2 -MemoryStartupBytes 2GB \
         -VHDPath {disk} -SwitchName {switch} -ErrorAction Stop;\n\
         Set-VM -Name {name} -ProcessorCount 2 -DynamicMemory:$false \
         -CheckpointType Disabled -AutomaticStartAction Nothing -AutomaticStopAction TurnOff;\n\
         Set-VMProcessor -VMName {name} -ExposeVirtualizationExtensions $false;\n\
         Set-VMKeyProtector -VMName {name} -NewLocalKeyProtector -ErrorAction SilentlyContinue",
        name = ps_literal(vm_name),
        disk = ps_literal(&disk_str),
        switch = ps_literal(&net_config.switch_name)
    ))
    .await?;

    // 5. Materialise the memory object so the first guest read is cheap.
    let _ = execute_ps_async(format!(
        "Get-VM -Name {} | Get-VMMemory | Out-Null",
        ps_literal(vm_name)
    ))
    .await;

    // 6. Boot.
    execute_ps_async(format!(
        "Start-VM -Name {} -ErrorAction Stop",
        ps_literal(vm_name)
    ))
    .await?;

    // 7. Wait for running state, then RDP readiness on loopback.
    wait_for_boot(vm_name).await?;
    verify_rdp_ready(&net_config.loopback_ip).await?;

    // 8. Final leak check before handing the session to the user.
    let net = net_config.clone();
    let leak = tokio::task::spawn_blocking(move || network::detect_route_leak(&net))
        .await
        .map_err(|e| Error::other(format!("leak check panicked: {}", e)))?
        .map_err(Error::other)?;

    if leak {
        let net = net_config.clone();
        tokio::task::spawn_blocking(move || network::remediate_leak(&net))
            .await
            .map_err(|e| Error::other(format!("leak remediation panicked: {}", e)))?
            .map_err(Error::other)?;
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

pub fn create_checkpoint(vm_name: &str, checkpoint_name: &str) -> Result<(), Error> {
    execute_ps(&format!(
        "Checkpoint-VM -Name {name} -SnapshotName {snap} -ErrorAction Stop",
        name = ps_literal(vm_name),
        snap = ps_literal(checkpoint_name)
    ))?;
    Ok(())
}

pub fn restore_checkpoint(vm_name: &str, checkpoint_name: &str) -> Result<(), Error> {
    execute_ps(&format!(
        "Restore-VMCheckpoint -Name {snap} -VMName {name} -Confirm:$false -ErrorAction Stop;\n\
         Start-VM -Name {name};",
        name = ps_literal(vm_name),
        snap = ps_literal(checkpoint_name)
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Destroy
// ---------------------------------------------------------------------------

/// Scorched earth. Kills the VM, deletes the differencing disk (all session
/// writes), and tears down network state. The base image survives untouched.
///
/// When `disk_path` is `None` the attached disks are resolved from the VM
/// record before removal — otherwise the delta VHDX leaks in ProgramData and
/// carries the session evidence with it.
pub fn destroy_vm(vm_name: &str, disk_path: Option<&Path>) -> Result<(), Error> {
    let name = ps_literal(vm_name);

    let resolved: Vec<String> = match disk_path {
        Some(p) => vec![p.to_string_lossy().to_string()],
        None => execute_ps(&format!(
            "$vm = Get-VM -Name {name} -ErrorAction SilentlyContinue; \
             if ($vm) {{ $vm.HardDrives | ForEach-Object {{ $_.Path }} }}",
            name = name
        ))
        .map(|out| {
            out.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default(),
    };

    execute_ps(&format!(
        "Stop-VM -Name {name} -TurnOff -Force -ErrorAction SilentlyContinue;\n\
         Remove-VM -Name {name} -Force -ErrorAction SilentlyContinue;",
        name = name
    ))?;

    // Nuke the delta disks — this is where all session activity (proctoring
    // app logs, browser history, exam artifacts) lives. Gone means gone.
    for path in resolved {
        let _ = clone::destroy_differencing_disk(Path::new(&path));
    }

    // Tear down network residue. Callers should NOT also call this.
    let _ = network::destroy_network(&network::get_network_config());

    Ok(())
}