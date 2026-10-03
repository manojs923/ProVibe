use tokio::time::{sleep, Duration};
use tokio::net::TcpStream;
use std::process::Command;
use std::io::{Error, ErrorKind};

// silent powershell executor for lifecycle commands
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
        "Restore-VM -Name '{}' -Name '{}' -Confirm:$false -ErrorAction Stop;\n\
         Start-VM -Name '{}';",
        vm_name, checkpoint_name, vm_name
    );
    execute_ps(&script)?;
    Ok(())
}

/// scorched earth. forcefully kills the process and deletes the vm.
pub fn destroy_vm(vm_name: &str) -> Result<(), Error> {
    let script = format!(
        "Stop-VM -Name '{name}' -TurnOff -Force -ErrorAction SilentlyContinue;\n\
         Remove-VM -Name '{name}' -Force -ErrorAction SilentlyContinue;",
        name = vm_name
    );
    execute_ps(&script)?;
    Ok(())
}