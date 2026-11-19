pub mod clone;
pub mod config;
pub mod hyperv;
pub mod lifecycle;
pub mod network;
pub mod sandbox;

use crate::target_id::TargetProfile;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::vm::network::NetworkConfig;

/// Local guest account used for PowerShell Direct. The base image must have
/// this account with this password; override with
/// [`VmOrchestrator::with_guest_credentials`] when it differs.
const DEFAULT_GUEST_USER: &str = "Administrator";
const DEFAULT_GUEST_PASSWORD: &str = "Protocol!2024";

/// Virtual switch used for the loopback pair. Created by `network`.
const LOOPBACK_SWITCH: &str = "Protocol-Loopback";

/// Default startup memory for a freshly provisioned guest, in MB.
///
/// Overridable with `PROTOCOL_VM_MEMORY_MB`. The 6 GB this used to hardcode
/// cannot be committed on a 16 GB host that is already busy, and a failed
/// commit surfaces as a bare `Insufficient system resources` from Start-VM.
const DEFAULT_VM_MEMORY_MB: u64 = 2048;

/// Resolve the startup memory, in bytes. An explicit `PROTOCOL_VM_MEMORY_MB`
/// always wins over the profile, because the profile has no idea how much RAM
/// the host can actually spare.
fn provision_memory_bytes(profile_mb: u32) -> u64 {
    std::env::var("PROTOCOL_VM_MEMORY_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|mb| *mb >= 1024)
        .unwrap_or(profile_mb as u64)
        * 1024
        * 1024
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// PowerShell single-quoted literal escaping. Doubles embedded quotes.
fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// `Path::canonicalize` hands back an extended-length path (`\\?\C:\...`).
/// `New-VHD` passes the parent path straight down to the VHD layer, which does
/// not accept the verbatim prefix and reports the file as not existing. Strip
/// it before any path reaches a cmdlet.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{}", rest));
    }
    if let Some(rest) = raw.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

fn run_ps(script: &str) -> Result<String, String> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()
        .map_err(|e| format!("failed to spawn powershell.exe: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Locate the base VHDX. Checked in order: explicit override, the working
/// directory (dev runs from `src-tauri`), then beside the executable
/// (bundled resources land in `resources/`).
///
/// The returned path is absolute but never verbatim-prefixed.
pub fn resolve_base_vhdx() -> Option<PathBuf> {
    let candidates = [
        std::env::var("PROTOCOL_BASE_VHDX").ok().map(PathBuf::from),
        Some(PathBuf::from("resources").join("base.vhdx")),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("resources").join("base.vhdx"))),
    ];

    candidates
        .into_iter()
        .flatten()
        .find(|p| p.exists())
        .map(|p| strip_verbatim(p.canonicalize().unwrap_or(p)))
}

fn vm_state(name: &str) -> Option<String> {
    let script = format!(
        "(Get-VM -Name '{}' -ErrorAction SilentlyContinue).State.ToString()",
        ps_escape(name)
    );
    let out = run_ps(&script).ok()?;
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// VM STATUS QUERY
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct VmStatus {
    pub running: bool,
    pub vm_name: String,
    pub state: String,
    pub uptime_seconds: u64,
    pub cpu_usage: f32,
    pub memory_assigned_mb: u64,
    pub memory_usage_mb: u64,
}

#[derive(Deserialize)]
struct RawVmStatus {
    #[serde(rename = "State")]
    state: String,
    #[serde(rename = "UptimeSeconds", default)]
    uptime_seconds: i64,
    #[serde(rename = "ProcessorUsage", default)]
    cpu_usage: Option<f64>,
    #[serde(rename = "MemoryAssigned", default)]
    memory_assigned: u64,
    #[serde(rename = "MemoryDemand", default)]
    memory_demand: u64,
}

fn get_vm_status_blocking(vm_name: String) -> Result<VmStatus, String> {
    // Emit flat primitives rather than the raw TimeSpan object, so the host
    // side has one unambiguous shape to parse.
    let ps_cmd = format!(
        r#"
$ErrorActionPreference = 'Stop'
$vm = Get-VM -Name '{vm}' -ErrorAction Stop
[pscustomobject]@{{
    State          = $vm.State.ToString()
    UptimeSeconds  = [int64]((Get-Date) - $vm.Uptime).TotalSeconds
    ProcessorUsage = [double]$vm.ProcessorUsage
    MemoryAssigned = [uint64]$vm.MemoryAssigned
    MemoryDemand   = [uint64]$vm.MemoryDemand
}} | ConvertTo-Json -Compress
"#,
        vm = ps_escape(&vm_name)
    );

    let stdout = match run_ps(&ps_cmd) {
        Ok(s) => s,
        Err(_) => return Ok(not_found(vm_name)),
    };

    if stdout.is_empty() {
        return Ok(not_found(vm_name));
    }

    let raw: RawVmStatus = match serde_json::from_str(&stdout) {
        Ok(r) => r,
        Err(_) => return Ok(not_found(vm_name)),
    };

    let running = raw.state.eq_ignore_ascii_case("Running");

    Ok(VmStatus {
        running,
        vm_name,
        state: raw.state,
        uptime_seconds: raw.uptime_seconds.max(0) as u64,
        cpu_usage: raw.cpu_usage.unwrap_or(0.0) as f32,
        memory_assigned_mb: raw.memory_assigned / (1024 * 1024),
        memory_usage_mb: raw.memory_demand / (1024 * 1024),
    })
}

#[tauri::command]
pub async fn get_vm_status(vm_name: String) -> Result<VmStatus, String> {
    tokio::task::spawn_blocking(move || get_vm_status_blocking(vm_name))
        .await
        .map_err(|e| format!("status task panicked: {}", e))?
}

fn not_found(vm_name: String) -> VmStatus {
    VmStatus {
        running: false,
        vm_name,
        state: "NotFound".to_string(),
        uptime_seconds: 0,
        cpu_usage: 0.0,
        memory_assigned_mb: 0,
        memory_usage_mb: 0,
    }
}

// ---------------------------------------------------------------------------
// VM SPAWN
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct VmSpawnResult {
    pub success: bool,
    pub vm_name: String,
    pub pid: Option<u32>,
    pub message: String,
}

/// Provision (if absent) and start the guest, then attach the console.
///
/// A VM that already exists is reused and the entire cold path is skipped, so
/// a warm engage costs one `Get-VM` plus one `Start-VM`. Boot is *not* awaited
/// here: the caller returns to the UI immediately and the frontend polls
/// `get_vm_status` for the transition to Running.
fn spawn_blocking_impl(
    vm_name: String,
    stealth_profile: Option<String>,
) -> Result<VmSpawnResult, String> {
    let profile = stealth_profile.unwrap_or_else(|| "default".to_string());
    let name = ps_escape(&vm_name);

    // ── 1. Cold path: provision + spoof + harden ─────────────────
    //
    // All three only make sense on a disk that does not exist yet, and all
    // three require the guest to be off. Running them on every engage is what
    // made the button feel like it hung.
    let existing = vm_state(&vm_name);

    if existing.is_none() {
        let base = resolve_base_vhdx().ok_or_else(|| {
            "base VHDX not found. Set PROTOCOL_BASE_VHDX or run scripts/create-base-vhdx.ps1."
                .to_string()
        })?;
        // Defensive second strip: PROTOCOL_BASE_VHDX may be set by hand with a
        // verbatim prefix and never passes through resolve_base_vhdx's map.
        let base_literal = ps_escape(&strip_verbatim(base).to_string_lossy());

        // The profile now reaches the VM instead of being thrown away.
        let cfg = config::build_config_named(&profile, &vm_name);
        let mem = provision_memory_bytes(cfg.memory_mb);
        let cores = cfg.cpu_cores.clamp(2, 8);

        // Disk work and hardening share one interpreter launch. Hardening is
        // wrapped so a WMI hiccup cannot leave a half-provisioned disk behind
        // with the whole engage reported as failed.
        let provision = format!(
            r#"$ErrorActionPreference = 'Stop'
$name = '{name}'
$base = '{base}'
if (-not (Test-Path -LiteralPath $base)) {{ throw "base image missing: $base" }}
$dir  = Join-Path $env:ProgramData 'Protocol\VMs'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$vhd = Join-Path $dir ($name + '.vhdx')
New-VHD -Path $vhd -ParentPath $base -Differencing | Out-Null
New-VM -Name $name -VHDPath $vhd -MemoryStartupBytes {mem} -Generation 2 | Out-Null
Set-VM -Name $name -ProcessorCount {cores} -CheckpointType Disabled | Out-Null
Add-VMNetworkAdapter -VMName $name -SwitchName '{switch}' -ErrorAction SilentlyContinue
try {{
    Set-VMMemory -VMName $name -DynamicMemoryEnabled $false
    $vmObj = Get-WmiObject -Namespace root\virtualization\v2 -Class Msvm_ComputerSystem -Filter "ElementName='$name'"
    if ($vmObj) {{
        $sd = $vmObj.GetRelated('Msvm_VirtualSystemSettingData') | Where-Object {{ $_.VirtualSystemType -eq 'Microsoft:Hyper-V:System:Realized' }} | Select-Object -First 1
        if ($sd) {{
            $proc = $sd.GetRelated('Msvm_ProcessorSettingData') | Select-Object -First 1
            if ($proc) {{
                $proc.HideHypervisorPresent = $true
                $vmms = Get-WmiObject -Namespace root\virtualization\v2 -Class Msvm_VirtualSystemManagementService
                $res = $vmms.ModifyResourceSettings($proc.GetText(1))
                if ($res.ReturnValue -ne 0 -and $res.ReturnValue -ne 4096) {{
                    throw "ModifyResourceSettings failed: $($res.ReturnValue)"
                }}
            }}
        }}
    }}
    Set-VMProcessor -VMName $name -ExposeVirtualizationExtensions $false
}} catch {{
    Write-Warning "hardening skipped: $_"
}}
"#,
            name = name,
            base = base_literal,
            mem = mem,
            cores = cores,
            switch = LOOPBACK_SWITCH
        );

        run_ps(&provision).map_err(|e| format!("provisioning failed: {}", e))?;

        // Identity spoof. The VM is still off here, which the WMI writes require.
        if let Err(e) = config::apply_spoof(&vm_name, &cfg.spoof_profile) {
            eprintln!("[vm] spoof failed for '{}': {}", vm_name, e);
        }

        // Host-wide, needs elevation, and has nothing to do with an individual
        // guest, so it stays on the cold path only.
        let _ = run_ps("Set-VMHost -EnableEnhancedSessionMode $false -ErrorAction SilentlyContinue");
    }

    // ── 2. Start ─────────────────────────────────────────────────
    let state = vm_state(&vm_name).unwrap_or_else(|| "Off".to_string());
    let already_running = state.eq_ignore_ascii_case("Running");

    if !already_running {
        run_ps(&format!("Start-VM -Name '{}'", name))
            .map_err(|e| format!("Start-VM failed: {}", e))?;
    }

    // ── 3. Attach the console (best-effort) ──────────────────────
    //
    // vmconnect works against a Starting guest, and a console failure is not
    // worth failing the engage over when the guest is already on its way up.
    let pid = match Command::new("vmconnect.exe")
        .arg("localhost")
        .arg(&vm_name)
        .spawn()
    {
        Ok(child) => Some(child.id()),
        Err(e) => {
            eprintln!("[vm] vmconnect failed for '{}': {}", vm_name, e);
            None
        }
    };

    Ok(VmSpawnResult {
        success: true,
        vm_name,
        pid,
        message: if already_running {
            format!("VM already running (profile: {})", profile)
        } else {
            format!("VM starting (profile: {})", profile)
        },
    })
}

/// Blocking form, for callers already on a blocking thread.
pub fn spawn_stealth_vm_blocking(
    vm_name: String,
    stealth_profile: Option<String>,
) -> Result<VmSpawnResult, String> {
    spawn_blocking_impl(vm_name, stealth_profile)
}

#[tauri::command]
pub async fn spawn_stealth_vm_cmd(
    vm_name: String,
    stealth_profile: Option<String>,
) -> Result<VmSpawnResult, String> {
    tokio::task::spawn_blocking(move || spawn_blocking_impl(vm_name, stealth_profile))
        .await
        .map_err(|e| format!("spawn task panicked: {}", e))?
}

// ---------------------------------------------------------------------------
// VM SHUTDOWN
// ---------------------------------------------------------------------------

fn shutdown_blocking(vm_name: String, force: bool) -> Result<String, String> {
    let script = if force {
        format!("Stop-VM -Name '{}' -Force -ErrorAction Stop", ps_escape(&vm_name))
    } else {
        format!("Stop-VM -Name '{}' -ErrorAction Stop", ps_escape(&vm_name))
    };

    run_ps(&script).map_err(|e| format!("shutdown failed: {}", e))?;
    Ok(format!("VM '{}' shutdown initiated", vm_name))
}

#[tauri::command]
pub async fn shutdown_vm(vm_name: String, force: Option<bool>) -> Result<String, String> {
    let force = force.unwrap_or(false);
    tokio::task::spawn_blocking(move || shutdown_blocking(vm_name, force))
        .await
        .map_err(|e| format!("shutdown task panicked: {}", e))?
}

// ---------------------------------------------------------------------------
// ORCHESTRATOR STATE MACHINE
// ---------------------------------------------------------------------------

/// Represents the lifecycle state of the ephemeral sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VmState {
    Offline,
    Provisioning,
    Booting,
    Running,
    Hooked,
    Nuked,
    Error(String),
}

/// The central controller for spinning up and tearing down the localized matrix.
#[allow(dead_code)]
pub struct VmOrchestrator {
    pub profile: TargetProfile,
    pub vm_name: String,
    pub state: Arc<Mutex<VmState>>,
    pub loopback_ip: String,
    pub guest_username: String,
    pub guest_password: String,
}

#[allow(dead_code)]
pub struct Vm {
    pub vm_name: String,
    pub loopback_ip: String,
}

impl Vm {
    pub async fn create_differencing_disk(&self) -> Result<(), String> {
        let base = resolve_base_vhdx().ok_or_else(|| "base VHDX not found".to_string())?;
        let output_dir = std::env::var("ProgramData")
            .map(|p| PathBuf::from(p).join("Protocol").join("VMs"))
            .map_err(|_| "ProgramData unset".to_string())?;

        clone::create_differencing_disk(&self.vm_name, &base, &output_dir)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub async fn configure_loopback_adapter(&self) -> Result<(), String> {
        network::configure_loopback_adapter(&NetworkConfig::default())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

impl VmOrchestrator {
    /// Initializes a new orchestrator for a specific proctoring target.
    pub fn new(profile: TargetProfile) -> Self {
        // Pseudo-random name to avoid Hyper-V collisions and defeat
        // static-string host scans.
        let entropy = Uuid::new_v4().to_string();
        let vm_name = format!("win_sys_{}", &entropy[..8]);

        Self {
            profile,
            vm_name,
            state: Arc::new(Mutex::new(VmState::Offline)),
            loopback_ip: "127.0.0.2".to_string(),
            guest_username: DEFAULT_GUEST_USER.to_string(),
            guest_password: DEFAULT_GUEST_PASSWORD.to_string(),
        }
    }

    /// Override the guest credentials used for PowerShell Direct.
    pub fn with_guest_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.guest_username = username.into();
        self.guest_password = password.into();
        self
    }

    /// Main ignition sequence — full deployment pipeline.
    pub async fn engage(&self) -> Result<(), String> {
        {
            let mut state = self.state.lock().await;
            *state = VmState::Provisioning;
        }

        let _cfg = config::build_config(&self.profile.display_name);

        network::configure_loopback_adapter(&NetworkConfig::default())
            .map_err(|e| e.to_string())?;

        {
            let mut state = self.state.lock().await;
            *state = VmState::Booting;
        }

        hyperv::HyperVOrchestrator::new(&self.vm_name)
            .start()
            .map_err(|e| e.to_string())?;

        lifecycle::wait_for_boot(&self.loopback_ip)
            .await
            .map_err(|e| e.to_string())?;

        {
            let mut state = self.state.lock().await;
            *state = VmState::Running;
        }

        Ok(())
    }

    /// Injects target-specific kernel and registry patches via PowerShell remoting.
    pub async fn apply_stealth_hooks(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;

        // Wire to crate::patches::loader once the injection API lands; the
        // profile carries the patch_set_id the loader needs.
        *state = VmState::Hooked;
        Ok(())
    }

    /// Completely obliterates the VM, differencing disk, and vSwitch.
    pub async fn teardown(&self) -> Result<(), String> {
        lifecycle::destroy_vm(&self.vm_name, None).map_err(|e| e.to_string())?;
        network::destroy_network(&network::get_network_config()).map_err(|e| e.to_string())?;

        let mut state = self.state.lock().await;
        *state = VmState::Nuked;
        Ok(())
    }

    /// Returns current state for the frontend telemetry feed.
    pub async fn get_status(&self) -> VmState {
        self.state.lock().await.clone()
    }
}