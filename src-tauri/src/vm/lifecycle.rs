use tokio::time::{sleep, Duration};
use tokio::net::TcpStream;
use std::process::Command;
use std::path::{Path, PathBuf};
use std::io::{Error, ErrorKind};

use crate::vm::clone;
use crate::vm::network;

/// silent powershell executor for lifecycle commands
fn execute_ps(script: &str) -> Result<String, Error> {
    let output = Command::new("powershell")
        .args(&["-NoProfile", "-NonInteractive", "-Command", script])
        .output()?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(Error::new(ErrorKind::Other, format!("ps execution failed: {}", err)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// blocks until the hyper-v wmi state confirms the vm is actually running
pub async fn wait_for_boot(vm_name: &str) -> Result<(), Error> {
    let script = format!("(Get-VM -Name '{}').State", vm_name);
    let mut retries = 0;

    while retries < 30 {
        if let Ok(state) = execute_ps(&script) {
            if state.trim().eq_ignore_ascii_case("Running") {
                return Ok(());
            }
        }
        sleep(Duration::from_secs(1)).await;
        retries += 1;
    }

    Err(Error::new(ErrorKind::TimedOut, "vm failed to reach running state. is hyper-v completely fucked?"))
}

/// actively polls the loopback rdp port (3389) until the guest os tcp stack is up
pub async fn verify_rdp_ready(ip: &str) -> Result<(), Error> {
    let addr = format!("{}:3389", ip);
    let mut retries = 0;

    // spam syn packets until the rdp service responds. usually takes 15-25 seconds after boot.
    while retries < 60 {
        match TcpStream::connect(&addr).await {
            Ok(_) => {
                // connection established. rdp is ready to be scraped by the python c2.
                return Ok(());
            }
            Err(_) => {
                sleep(Duration::from_millis(500)).await;
                retries += 1;
            }
        }
    }

    Err(Error::new(ErrorKind::TimedOut, "rdp port 3389 never opened. check the vswitch routing."))
}

/// full spawn pipeline: network isolation -> differencing disk -> vm creation -> boot -> rdp poll.
/// this is the function the tauri command calls on Alt+V.
pub async fn spawn_stealth_vm(
    vm_name: &str,
    base_image: &Path,
    vm_dir: &Path,
) -> Result<SpawnedVm, Error> {
    let net_config = network::get_network_config();
    let disk_dir = vm_dir.join("disks");

    // 1. network: isolated private switch + loopback at 127.0.0.2
    network::setup_network(&net_config)
        .map_err(|e| Error::new(ErrorKind::Other, format!("network setup failed: {}", e)))?;

    // 2. disk: differencing vhdx referencing the golden image
    let disk = clone::create_differencing_disk(vm_name, base_image, &disk_dir)
        .map_err(|e| Error::new(ErrorKind::Other, format!("disk clone failed: {}", e)))?;

    // 3. destroy stale vm with same name if a previous session crashed mid-teardown
    let _ = execute_ps(&format!(
        "Stop-VM -Name '{}' -TurnOff -Force -ErrorAction SilentlyContinue;\n\
         Remove-VM -Name '{}' -Force -ErrorAction SilentlyContinue",
        vm_name, vm_name
    ));

    // 4. create the vm: gen2, no dynamic memory (deterministic memory footprint),
    //    minimum specs since it's just running a proctoring app and rdp.
    let disk_str = disk.disk_path.to_string_lossy().to_string();
    execute_ps(&format!(
        "New-VM -Name '{name}' -Generation 2 -MemoryStartupBytes 2GB \
         -VHDPath '{disk}' -SwitchName '{switch}' -ErrorAction Stop;\n\
         Set-VM -Name '{name}' -ProcessorCount 2 -DynamicMemory:$false \
         -CheckpointType Disabled -AutomaticStartAction Nothing -AutomaticStopAction TurnOff;\n\
         Set-VMProcessor -VMName '{name}' -ExposeVirtualizationExtensions $false;\n\
         Set-VMKeyProtector -VMName '{name}' -NewLocalKeyProtector -ErrorAction SilentlyContinue",
        name = vm_name,
        disk = disk_str,
        switch = net_config.switch_name
    ))?;

    // 5. disable the vm-serial/keyboard-ps2 devices that leak host telemetry patterns
    //    in the hypervisor event log. keep it quiet.
    let _ = execute_ps(&format!(
        "Get-VM -Name '{}' | Get-VMMemory | Out-Null", vm_name
    ));

    // 6. boot
    execute_ps(&format!("Start-VM -Name '{}' -ErrorAction Stop", vm_name))?;

    // 7. wait for running state, then rdp readiness on loopback
    wait_for_boot(vm_name).await?;
    verify_rdp_ready(&net_config.loopback_ip).await?;

    // 8. final leak check before handing the session to the user
    let leak = network::detect_route_leak(&net_config)
        .map_err(|e| Error::new(ErrorKind::Other, e))?;
    if leak {
        network::remediate_leak(&net_config)
            .map_err(|e| Error::new(ErrorKind::Other, e))?;
    }

    Ok(SpawnedVm {
        name: vm_name.to_string(),
        disk_path: disk.disk_path,
        rdp_address: net_config.loopback_ip,
    })
}

/// result struct handed back to the tauri frontend
#[derive(Debug, Clone)]
pub struct SpawnedVm {
    pub name: String,
    pub disk_path: PathBuf,
    pub rdp_address: String,
}

/// creates a hyper-v checkpoint (snapshot) for instant redeployment
pub fn create_checkpoint(vm_name: &str, checkpoint_name: &str) -> Result<(), Error> {
    let script = format!(
        "Checkpoint-VM -Name '{}' -SnapshotName '{}' -ErrorAction Stop",
        vm_name, checkpoint_name
    );
    execute_ps(&script)?;
    Ok(())
}

/// obliterates current state and instantly reverts to the clean snapshot
pub fn restore_checkpoint(vm_name: &str, checkpoint_name: &str) -> Result<(), Error> {
    let script = format!(
        "Restore-VMSnapshot -Name '{snap}' -VMName '{name}' -Confirm:$false -ErrorAction Stop;\n\
         Start-VM -Name '{name}';",
        name = vm_name,
        snap = checkpoint_name
    );
    execute_ps(&script)?;
    Ok(())
}

/// scorched earth. forcefully kills the process, deletes the vm, destroys the differencing
/// disk (all session writes), and tears down network state. base image survives untouched.
pub fn destroy_vm(vm_name: &str, disk_path: Option<&Path>) -> Result<(), Error> {
    let script = format!(
        "Stop-VM -Name '{name}' -TurnOff -Force -ErrorAction SilentlyContinue;\n\
         Remove-VM -Name '{name}' -Force -ErrorAction SilentlyContinue;",
        name = vm_name
    );
    execute_ps(&script)?;

    // nuke the delta disk — this is where all session activity (proctoring app logs,
    // browser history, exam artifacts) lives. gone means gone.
    if let Some(path) = disk_path {
        let _ = clone::destroy_differencing_disk(path);
    }

    // tear down network residue
    let _ = network::destroy_network(&network::get_network_config());

    Ok(())
}
#[derive(Debug, Clone)]
pub struct VmHandle {
    pub name: String,
    pub pid: Option<u32>,
}