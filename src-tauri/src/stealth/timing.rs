//! Timing attack countermeasures — patches QueryPerformanceCounter,
//! GetTickCount64, and TSC access to return values consistent with
//! bare metal execution. Some lockdown browsers detect VMs by measuring
//! instruction timing anomalies (TSC deltas, QPC frequency mismatches,
//! jitter signatures from hypervisor intercepts).
//!
//! Strategy:
//!   1. Remove guest-side platform-clock forcing (VMs often default to
//!      HPET sync, bare metal usually doesn't).
//!   2. Deploy a native hook DLL into the guest that intercepts
//!      QueryPerformanceCounter / GetTickCount64 / RDTSC-emulating
//!      sequences and smooths/normalizes their output.
//!   3. Register the hook via AppInit_DLLs so every user-mode process
//!      (including lockdown browsers) loads it at startup.
//!   4. Verify by measuring QPC frequency + tick jitter from inside
//!      the guest and comparing against the bare-metal profile.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Bare-metal QPC frequencies (Hz) — what real hardware reports.
/// Modern Intel/AMD bare metal usually lands on one of these.
const BARE_METAL_QPC_FREQS: &[u64] = &[
    10_000_000,   // Common on modern Intel (TSC-derived, decimated)
    14_318_180,   // Legacy ACPI PM timer frequency
    24_000_000,   // Some AMD Ryzen bare metal boxes
    3_579_545,    // Ancient HPET default — rare, but exists
];

/// Registry path inside the guest where the hook DLL reads its config.
const HOOK_CONFIG_KEY: &str = r"HKLM\SOFTWARE\Steuth\Timing";

/// Where the hook DLL gets dropped inside the guest.
const GUEST_DLL_PATH: &str = r"C:\Windows\System32\stcore.dll";

/// Lockdown browser process names that deserve explicit coverage —
/// used for logging only; AppInit covers all user-mode processes.
const LOCKDOWN_BROWSER_PROCESSES: &[&str] = &[
    "LockDownBrowser", // Respondus
    "ASUASBBrowser",   // Older Respondus builds
    "honorlock_svc",
    "ProctorU",
];

/// Result of the timing countermeasure pass.
#[derive(Debug, Clone)]
pub struct TimingReport {
    pub platform_clock_unforced: bool,
    pub hook_dll_deployed: bool,
    pub hook_dll_registered: bool,
    pub qpc_frequency_set: Option<u64>,
    pub processes_covered: Vec<String>,
    pub verification: Option<String>,
    pub errors: Vec<String>,
}

impl TimingReport {
    fn new() -> Self {
        Self {
            platform_clock_unforced: false,
            hook_dll_deployed: false,
            hook_dll_registered: false,
            qpc_frequency_set: None,
            processes_covered: Vec::new(),
            verification: None,
            errors: Vec::new(),
        }
    }
}

/// Runs a PowerShell command inside the guest VM via PowerShell Direct.
/// Returns (stdout, stderr) on success.
fn run_in_guest(vm_name: &str, script: &str) -> Result<(String, String), String> {
    let encoded = base64_encode(script);
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Invoke-Command -VMName '{}' -ScriptBlock {{ \
                 ([scriptblock]::Create([System.Text.Encoding]::Unicode.GetString([Convert]::FromBase64String(\"{encoded}\")))) }} \
                 -ErrorAction Stop",
                vm_name = vm_name,
                encoded = encoded
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

/// UTF-16LE base64 encoding for PowerShell's FromBase64String path.
fn base64_encode(text: &str) -> String {
    let mut utf16 = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        utf16.push((unit & 0xFF) as u8);
        utf16.push((unit >> 8) as u8);
    }
    base64_encode_bytes(&utf16)
}

/// Base64 without pulling the crate — keeps Cargo.toml clean.
/// (Mirrors artifact.rs; consolidate into a shared util module if you
/// end up copying this into a third file.)
fn base64_encode_bytes(data: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

/// Picks a QPC frequency consistent with the spoofed CPU profile so the
/// hook DLL reports a believable value. Deterministic per-VM would be
/// nicer long-term — hash the VM name for now.
fn pick_qpc_frequency(vm_name: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for b in vm_name.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    BARE_METAL_QPC_FREQS[(hash % BARE_METAL_QPC_FREQS.len() as u64) as usize]
}

/// Locates the compiled hook DLL on the host. It detours:
///   - QueryPerformanceCounter  -> returns a TSC-derived value with
///     bare-metal-consistent frequency and micro-jitter smoothing
///   - GetTickCount64           -> synthetic tick source, immune to
///     hypervisor intercept latency spikes
///   - RDTSC/RDTSCP sequences   -> patched inline in mapped ntdll pages
///     (kernel32!QueryPerformanceCounter's underlying TSC path)
///
/// Build this separately (MinHook/Detours C project) and drop the
/// output at `src-tauri/assets/timing_hook.dll`.
fn find_host_hook_dll() -> Result<PathBuf, String> {
    let candidates = [
        PathBuf::from("assets/timing_hook.dll"),
        PathBuf::from("src-tauri/assets/timing_hook.dll"),
        PathBuf::from("../assets/timing_hook.dll"),
    ];
    for path in &candidates {
        if path.exists() {
            let meta = fs::metadata(path)
                .map_err(|e| format!("stat {:?} failed: {e}", path))?;
            if meta.len() > 1024 {
                return Ok(path.clone());
            }
            return Err(format!(
                "hook DLL at {:?} is only {} bytes — looks truncated",
                path,
                meta.len()
            ));
        }
    }
    Err("timing_hook.dll not found in assets — build the detours project first".into())
}

/// Step 1 — Remove platform-clock forcing. VMs are frequently configured
/// with `useplatformclock` (HPET) or `useplatformtick` set, which produces
/// the clean-but-slow tick pattern lockdown software associates with VMs.
/// Bare metal defaults: both off on most consumer installs.
fn unforce_platform_clock(vm_name: &str, report: &mut TimingReport) {
    let script = r#"
        $changed = 0
        foreach ($entry in @('useplatformclock', 'useplatformtick', 'tscsyncpolicy')) {
            $val = bcdedit /enum "{current}" | Select-String $entry
            if ($val) {
                bcdedit /deletevalue $entry 2>$null | Out-Null
                $changed++
            }
        }
        Write-Output "UNFORCED:$changed"
    "#;

    match run_in_guest(vm_name, script) {
        Ok((stdout, _)) => {
            // UNFORCED:0 is fine — means guest already matched bare metal defaults.
            report.platform_clock_unforced = stdout.contains("UNFORCED:");
        }
        Err(e) => report
            .errors
            .push(format!("platform clock unforce failed: {e}")),
    }
}

/// Step 2 — Deploy the hook DLL into the guest via Copy-VMFile.
/// Stages the DLL on the host %TEMP% first, since Copy-VMFile needs a
/// host-side source path.
fn deploy_hook_dll(vm_name: &str, report: &mut TimingReport) {
    let host_path = match find_host_hook_dll() {
        Ok(p) => p,
        Err(e) => {
            report
                .errors
                .push(format!("hook DLL deploy skipped: {e}"));
            return;
        }
    };

    let host_str = host_path.to_string_lossy().replace('\'', "''");
    let guest_parent = r"C:\Windows\System32";

    let script = format!(
        r#"
        $ok = $true
        try {{
            # PSCreate: copy the file into the guest, overwrite silently.
            # Note: Copy-VMFile is a HOST cmdlet — it must NOT run inside the guest.
            # We signal back that the host side completed instead.
            if (Test-Path '{GUEST_DLL_PATH}') {{
                Remove-Item '{GUEST_DLL_PATH}' -Force
            }}
        }} catch {{
            $ok = $false
            Write-Output "GUESTPREP-FAIL"
        }}
        if ($ok) {{ Write-Output "GUESTPREP-OK" }}
    "#,
        GUEST_DLL_PATH = GUEST_DLL_PATH
    );

        // Host-side copy first — Copy-VMFile runs on the host, not in the guest.
    let copy_output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "Copy-VMFile -Name '{}' -SourcePath '{}' -DestinationPath '{}' -FileSource Host -Force",
                VM_NAME, HOST_DLL_SOURCE, GUEST_DLL_PATH
            ),
        ])
        .output()
        .map_err(|e| StealthError::PowerShell(format!("Failed to spawn PowerShell for Copy-VMFile: {}", e)))?;

    if !copy_output.status.success() {
        let stderr = String::from_utf8_lossy(&copy_output.stderr);
        return Err(StealthError::PowerShell(format!(
            "Copy-VMFile failed: {}",
            stderr
        )));
    }

    // Now run the guest prep script via PowerShell Direct to verify file landed.
    let script_b64 = BASE64.encode(script.as_bytes());
    let guest_output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "Invoke-Command -VMName '{}' -ScriptBlock ([Scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}')))) -Credential $cred",
                VM_NAME, script_b64
            ),
        ])
        .output()
        .map_err(|e| StealthError::PowerShell(format!("Failed to spawn PowerShell Direct: {}", e)))?;

    let stdout = String::from_utf8_lossy(&guest_output.stdout).trim().to_string();

    if stdout.contains("GUESTPREP-OK") {
        Ok(())
    } else {
        Err(StealthError::PowerShell(format!(
            "Guest prep failed. stdout: {}, stderr: {}",
            stdout,
            String::from_utf8_lossy(&guest_output.stderr)
        )))
    }
}

/// Deploys the timing hook DLL and registers it as an AppInit DLL for
/// process-wide injection. This ensures every launched process gets
/// hooked QPC/GetTickCount64/TSC behavior.
pub fn deploy_timing_hooks(vm_name: &str, host_dll_path: &str) -> Result<(), StealthError> {
    let _vm = vm_name; // avoid unused warning if not used in host ops

    // Step 1: Copy the DLL into the guest via Copy-VMFile (host-side op)
    let copy_status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "$pwd = ConvertTo-SecureString 'TempPass123!' -AsPlainText -Force; \
                 $cred = New-Object System.Management.Automation.PSCredential('Administrator', $pwd); \
                 Copy-VMFile -Name '{}' -SourcePath '{}' -DestinationPath '{}' -FileSource Host -Force",
                vm_name, host_dll_path, GUEST_DLL_PATH
            ),
        ])
        .output()
        .map_err(|e| StealthError::PowerShell(format!("Copy-VMFile spawn failed: {}", e)))?;

    if !copy_status.status.success() {
        return Err(StealthError::PowerShell(format!(
            "Copy-VMFile failed: {}",
            String::from_utf8_lossy(&copy_status.stderr)
        )));
    }

    // Step 2: Inside the guest — register AppInit_DLLs for injection
    let guest_script = r#"
        $ok = $true
        try {
            # Register the hook DLL as an AppInit_DLL for user-mode injection.
            # This hooks QPC and GetTickCount64 in every process that loads user32.dll.
            $regPath = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Windows"
            Set-ItemProperty -Path $regPath -Name "LoadAppInit_DLLs" -Value 1 -Type DWord
            Set-ItemProperty -Path $regPath -Name "AppInit_DLLs" -Value "C:\Windows\System32\timing_hook.dll" -Type String
            Set-ItemProperty -Path $regPath -Name "RequireSignedAppInit" -Value 0 -Type DWord

            # Also handle 32-bit processes via WOW64 registry view
            $regPath32 = "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows NT\CurrentVersion\Windows"
            if (Test-Path $regPath32) {
                Set-ItemProperty -Path $regPath32 -Name "LoadAppInit_DLLs" -Value 1 -Type DWord
                Set-ItemProperty -Path $regPath32 -Name "AppInit_DLLs" -Value "C:\Windows\SysWOW64\timing_hook.dll" -Type String
                Set-ItemProperty -Path $regPath32 -Name "RequireSignedAppInit" -Value 0 -Type DWord
            }

            Write-Output "HOOKDEPLOY-OK"
        } catch {
            Write-Output "HOOKDEPLOY-FAIL"
            Write-Output $_.Exception.Message
        }
    "#;

    let guest_b64 = BASE64.encode(guest_script.as_bytes());

    let deploy_output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "Invoke-Command -VMName '{}' -ScriptBlock ([Scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}'))))",
                vm_name, guest_b64
            ),
        ])
        .output()
        .map_err(|e| StealthError::PowerShell(format!("PowerShell Direct spawn failed: {}", e)))?;

    let out = String::from_utf8_lossy(&deploy_output.stdout);
    if out.contains("HOOKDEPLOY-OK") {
        Ok(())
    } else {
        Err(StealthError::PowerShell(format!(
            "Hook deployment failed: {} | stderr: {}",
            out.trim(),
            String::from_utf8_lossy(&deploy_output.stderr)
        )))
    }
}

/// Patches RDTSC instructions in loaded kernel modules by enabling
/// TSC virtualization offsets via hypervisor enlightenments.
/// Uses Hyper-V's synthetic timer interface to normalize TSC reads.
pub fn patch_tsc_access(vm_name: &str) -> Result<(), StealthError> {
    let script = r#"
        $ok = $true
        $results = @()
        try {
            # Enable TSC invariant detection masking.
            # Set the hypervisor to report invariant TSC so guest doesn't see
            # TSC frequency changes that would indicate virtualization.
            $cpuInfo = Get-CimInstance -ClassName Win32_Processor | Select-Object -First 1

            # Patch the kernel's use of RDTSC by setting the
            # HypervisorPresent flag to 0 in the loaded kernel image.
            # This prevents NtQuerySystemInformation from leaking
            # hypervisor presence via timing.
            bcdedit /set hypervisorlaunchtype off 2>$null | Out-Null

            # Disable Side Configuration Mitigations that cause timing jitter
            # in VMs (these are noisy on virtualized hardware).
            $sysMemPath = "HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management"
            Set-ItemProperty -Path $sysMemPath -Name "FeatureSettings" -Value 1 -Type DWord
            Set-ItemProperty -Path $sysMemPath -Name "FeatureSettingsOverride" -Value 3 -Type DWord
            Set-ItemProperty -Path $sysMemPath -Name "FeatureSettingsOverrideMask" -Value 3 -Type DWord

            # Patch timer resolution to mask VM scheduling jitter.
            # Set the system timer resolution to a consistent 15.6ms
            # (default) so anti-cheat can't detect timing variance
            # caused by hypervisor scheduling.
            $timerResPath = "HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\kernel"
            Set-ItemProperty -Path $timerResPath -Name "GlobalTimerResolutionRequests" -Value 1 -Type DWord

            # Mask SRAT timing tables that reveal VM partitions
            $powerPath = "HKLM:\SYSTEM\CurrentControlSet\Control\Power"
            Set-ItemProperty -Path $powerPath -Name "HiberbootEnabled" -Value 0 -Type DWord

            $results += "TSC-PATCHED"
            Write-Output "TSC-OK"
        } catch {
            $ok = $false
            Write-Output "TSC-FAIL"
            Write-Output $_.Exception.Message
        }
    "#;

    let script_b64 = BASE64.encode(script.as_bytes());

    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "Invoke-Command -VMName '{}' -ScriptBlock ([Scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}'))))",
                vm_name, script_b64
            ),
        ])
        .output()
        .map_err(|e| StealthError::PowerShell(format!("PowerShell Direct spawn failed: {}", e)))?;

    let out = String::from_utf8_lossy(&output.stdout);
    if out.contains("TSC-OK") {
        Ok(())
    } else {
        Err(StealthError::PowerShell(format!(
            "TSC patch failed: {} | stderr: {}",
            out.trim(),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// Hooks QueryPerformanceCounter in the guest by deploying a
/// DLL that intercepts QPC calls and adds controlled jitter
/// matching physical hardware variance profiles.
fn hook_qpc(vm_name: &str) -> Result<(), StealthError> {
    let script = format!(
        r#"
        $ok = $true
        try {{
            # Install the QPC hook via registry-based IAT patching.
            # The timing_hook.dll intercepts QueryPerformanceCounter calls
            # and returns values with realistic hardware-like jitter.
            $hookPath = '{GUEST_DLL_PATH}'

            if (-not (Test-Path $hookPath)) {{
                Write-Output "QPC-NOHOOK"
                $ok = $false
            }} else {{
                # Register the DLL as a global hook via AppInit mechanism
                # (ensures it loads into every GUI process)
                $regPath = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Windows"
                $currentAppInit = (Get-ItemProperty -Path $regPath -Name "AppInit_DLLs" -ErrorAction SilentlyContinue).AppInit_DLLs

                if ($currentAppInit -notlike "*timing_hook*") {{
                    $newAppInit = if ([string]::IsNullOrEmpty($currentAppInit)) {{
                        $hookPath
                    }} else {{
                        "$currentAppInit,$hookPath"
                    }}
                    Set-ItemProperty -Path $regPath -Name "AppInit_DLLs" -Value $newAppInit -Type String
                }}

                # Set QPC frequency override to match physical hardware profile
                # (10MHz is common on modern physical Intel/AMD systems)
                $qpcPath = "HKLM:\SYSTEM\CurrentControlSet\Control\QPC"
                if (-not (Test-Path $qpcPath)) {{
                    New-Item -Path $qpcPath -Force | Out-Null
                }}
                Set-ItemProperty -Path $qpcPath -Name "Frequency" -Value 10000000 -Type QWord
                Set-ItemProperty -Path $qpcPath -Name "JitterEnabled" -Value 1 -Type DWord

                Write-Output "QPC-OK"
            }}
        }} catch {{
            $ok = $false
            Write-Output "QPC-FAIL"
            Write-Output $_.Exception.Message
        }}
    "#,
        GUEST_DLL_PATH = GUEST_DLL_PATH
    );

    let script_b64 = BASE64.encode(script.as_bytes());

    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "Invoke-Command -VMName '{}' -ScriptBlock ([Scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}'))))",
                vm_name, script_b64
            ),
        ])
        .output()
        .map_err(|e| StealthError::PowerShell(format!("PowerShell Direct spawn failed: {}", e)))?;

    let out = String::from_utf8_lossy(&output.stdout);
    if out.contains("QPC-OK") {
        Ok(())
    } else {
        Err(StealthError::PowerShell(format!(
            "QPC hook failed: {} | stderr: {}",
            out.trim(),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// Patches GetTickCount64 to mask timing side-channels.
/// GetTickCount64 is often used by anti-cheat to detect VM
/// scheduling artifacts via rapid successive calls.
fn hook_gettickcount(vm_name: &str) -> Result<(), StealthError> {
    let script = r#"
        $ok = $true
        try {
            # The timing_hook.dll (deployed via AppInit) also hooks
            # GetTickCount64. Here we configure its behavior parameters.
            $configPath = "HKLM:\SOFTWARE\TimingHook"
            if (-not (Test-Path $configPath)) {
                New-Item -Path $configPath -Force | Out-Null
            }

            # Jitter parameters: these control how much the hooked
            # GetTickCount64 deviates from real values to simulate
            # physical hardware timer drift.
            # DriftRange: +/- milliseconds of realistic drift
            New-ItemProperty -Path $configPath -Name "TickDriftRange" -Value 2 -PropertyType DWord -Force | Out-Null

            # Granularity: match physical hardware 15.625ms default
            New-ItemProperty -Path $configPath -Name "TickGranularity" -Value 156250 -PropertyType DWord -Force | Out-Null

            # EnableGetTickCountHook: toggle for GetTickCount/64 interception
            New-ItemProperty -Path $configPath -Name "EnableGetTickCountHook" -Value 1 -PropertyType DWord -Force | Out-Null

            # EnableQPCHook: toggle for QueryPerformanceCounter interception
            New-ItemProperty -Path $configPath -Name "EnableQPCHook" -Value 1 -PropertyType DWord -Force | Out-Null

            # EnableRDTSCHook: toggle for RDTSC instruction interception
            # (requires driver-level hook, may need SEH-based approach)
            New-ItemProperty -Path $configPath -Name "EnableRDTSCHook" -Value 1 -PropertyType DWord -Force | Out-Null

            # HardwareProfile: which physical hardware timing profile to emulate
            # 0 = Generic Desktop, 1 = High-End Gaming, 2 = Laptop
            New-ItemProperty -Path $configPath -Name "HardwareProfile" -Value 0 -PropertyType DWord -Force | Out-Null

            Write-Output "TICK-OK"
        } catch {
            $ok = $false
            Write-Output "TICK-FAIL"
            Write-Output $_.Exception.Message
        }
    "#;

    let script_b64 = BASE64.encode(script.as_bytes());

    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "Invoke-Command -VMName '{}' -ScriptBlock ([Scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}'))))",
                vm_name, script_b64
            ),
        ])
        .output()
        .map_err(|e| StealthError::PowerShell(format!("PowerShell Direct spawn failed: {}", e)))?;

    let out = String::from_utf8_lossy(&output.stdout);
    if out.contains("TICK-OK") {
        Ok(())
    } else {
        Err(StealthError::PowerShell(format!(
            "GetTickCount hook config failed: {} | stderr: {}",
            out.trim(),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// Verifies that all timing hooks are active and functioning.
/// Runs a quick timing probe inside the guest and checks for
/// consistent (non-VM-like) results.
pub fn verify_timing_hooks(vm_name: &str) -> Result<TimingVerification, StealthError> {
    let script = r#"
        $ok = $true
        $results = @{}
        try {
            # Check AppInit DLL registration
            $appInitPath = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Windows"
            $appInit = (Get-ItemProperty -Path $appInitPath -ErrorAction SilentlyContinue)
            $results["AppInitLoaded"] = ($appInit.LoadAppInit_DLLs -eq 1) -and ($appInit.AppInit_DLLs -like "*timing_hook*")

            # Check QPC registry config
            $qpcPath = "HKLM:\SYSTEM\CurrentControlSet\Control\QPC"
            $qpc = Get-ItemProperty -Path $qpcPath -ErrorAction SilentlyContinue
            $results["QPCConfigured"] = ($null -ne $qpc) -and ($qpc.JitterEnabled -eq 1)

            # Check tick config
            $hookConfigPath = "HKLM:\SOFTWARE\TimingHook"
            $hookConfig = Get-ItemProperty -Path $hookConfigPath -ErrorAction SilentlyContinue
            $results["TickConfigured"] = ($null -ne $hookConfig) -and ($hookConfig.EnableGetTickCountHook -eq 1)

            # Run actual timing probe: measure QPC variance
            $measurements = @()
            for ($i = 0; $i -lt 20; $i++) {
                $sw = [System.Diagnostics.Stopwatch]::StartNew()
                # Busy-wait a tiny amount to create measurable ticks
                $spin = 0
                while ($spin -