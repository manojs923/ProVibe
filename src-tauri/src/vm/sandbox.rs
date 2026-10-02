use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::io::{Error, ErrorKind};

/// orchestrates the lightweight windows sandbox (.wsb) fallback
pub struct SandboxOrchestrator {
    wsb_path: PathBuf,
    payload_dir: PathBuf,
}

impl SandboxOrchestrator {
    pub fn new(target_id: &str) -> Self {
        let temp_dir = std::env::temp_dir();
        Self {
            // drop the wsb file in appdata/temp so it doesn't leave obvious forensic turds
            wsb_path: temp_dir.join(format!("{}_ephemeral.wsb", target_id.to_lowercase())),
            // this is where we stage our memory patches and powershell scripts on the host
            payload_dir: temp_dir.join("stealth_payloads"),
        }
    }

    /// generates the xml config for the sandbox.
    pub fn generate_config(&self, use_vgpu: bool) -> Result<(), Error> {
        // ensure our host-side payload directory exists so the mapping doesn't shit the bed
        fs::create_dir_all(&self.payload_dir)?;

        // sometimes disabling vgpu helps bypass basic vm checks that look for hyper-v video drivers
        let vgpu_state = if use_vgpu { "Enable" } else { "Disable" };
        
        // the .wsb xml payload. we map the host payload folder as read-only and execute our hooks on boot.
        let xml = format!(
            r#"<Configuration>
  <VGpu>{vgpu}</VGpu>
  <Networking>Enable</Networking>
  <MappedFolders>
    <MappedFolder>
      <HostFolder>{host_folder}</HostFolder>
      <SandboxFolder>C:\stealth_payloads</SandboxFolder>
      <ReadOnly>true</ReadOnly>
    </MappedFolder>
  </MappedFolders>
  <LogonCommand>
    <Command>cmd.exe /c "C:\stealth_payloads\init_hooks.bat"</Command>
  </LogonCommand>
</Configuration>"#,
            vgpu = vgpu_state,
            host_folder = self.payload_dir.display()
        );

        fs::write(&self.wsb_path, xml)?;
        Ok(())
    }

    /// detonates the .wsb file, letting the os native handler spin up the container
    pub fn launch(&self) -> Result<(), Error> {
        // we just pass it to cmd /c start because windows natively associates .wsb files
        let output = Command::new("cmd")
            .args(&["/c", "start", "", self.wsb_path.to_str().unwrap()])
            .output()?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(Error::new(ErrorKind::Other, format!("failed to pop the sandbox container: {}", err)));
        }
        
        Ok(())
    }

    /// cleans up the config file. the sandbox itself is ephemeral and self-destructs on close.
    pub fn teardown(&self) -> Result<(), Error> {
        if self.wsb_path.exists() {
            fs::remove_file(&self.wsb_path)?;
        }
        Ok(())
    }
}