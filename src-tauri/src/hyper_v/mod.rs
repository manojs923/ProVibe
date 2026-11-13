//! src-tauri/src/hyper_v/mod.rs
//!
//! Host-side Hyper-V plumbing.
//!
//! This module is the *host* half of guest interaction: it shells out to the
//! Hyper-V PowerShell surface and never touches guest memory. Guest work is
//! delivered with PowerShell Direct (`Invoke-Command -VMName`), so nothing here
//! depends on WinRM, SMB, RDP, or the guest firewall posture.
//!
//! Dependency direction: `stealth` and `patches` may use this module; this
//! module must not know about either of them beyond `StealthError`.
//!
//! Scope rules:
//!   - `vm_manager` owns the trait and its implementation.
//!   - Everything shared by more than one call site (process invocation,
//!     credential shaping, script transport) lives here.
//!   - Anything that blocks is async and says so.

pub mod vm_manager;

use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::stealth::StealthError;
use crate::util::ps_literal;

pub use vm_manager::{VmManager, VmManagerOps};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Hard ceiling for a host PowerShell invocation.
const HOST_PS_TIMEOUT: Duration = Duration::from_secs(180);

/// Longer ceiling for file transfers, which move real payloads.
const HOST_PS_TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);

/// Shared argument vector for every host invocation. The script travels over
/// stdin (`-Command -`) so command-line length limits and quoting rules never
/// come into play.
const PS_ARGS: [&str; 6] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
    "-",
];

/// Local guest account used for PowerShell Direct. The base image must carry
/// this account; override with [`GuestCredentials::new`] when it differs.
pub const DEFAULT_GUEST_USER: &str = r".\Administrator";

/// Matching default password.
pub const DEFAULT_GUEST_PASSWORD: &str = "Protocol!2024";

// ---------------------------------------------------------------------------
// Guest credentials
// ---------------------------------------------------------------------------

/// Credentials for PowerShell Direct into a guest.
///
/// PowerShell Direct authenticates against the guest's local SAM, so the
/// username is deliberately machine-qualified (`.\Administrator`) rather than
/// a bare name — a bare name resolves against the host's domain first.
#[derive(Debug, Clone)]
pub struct GuestCredentials {
    username: String,
    password: String,
}

impl GuestCredentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        let mut username = username.into();
        // Force machine-qualification so the credential can never resolve
        // against the host domain.
        if !username.contains('\\') && !username.contains('@') {
            username = format!(r".\{}", username);
        }

        Self {
            username,
            password: password.into(),
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }
}

impl Default for GuestCredentials {
    fn default() -> Self {
        Self::new(DEFAULT_GUEST_USER, DEFAULT_GUEST_PASSWORD)
    }
}

// ---------------------------------------------------------------------------
// Host invocation
// ---------------------------------------------------------------------------

/// Run a host PowerShell script and return trimmed stdout.
///
/// `script` is fed over stdin and the process is hard-bounded by
/// `HOST_PS_TIMEOUT`. A non-zero exit becomes
/// [`StealthError::PowerShellFailure`] carrying the child's stderr, so callers
/// see the actual PowerShell error instead of a bare exit code.
pub(crate) async fn run_host_ps(script: &str) -> Result<String, StealthError> {
    run_host_ps_with_timeout(script, HOST_PS_TIMEOUT).await
}

/// As [`run_host_ps`], with an explicit ceiling.
pub(crate) async fn run_host_ps_with_timeout(
    script: &str,
    ceiling: Duration,
) -> Result<String, StealthError> {
    let mut child = Command::new("powershell.exe")
        .args(PS_ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            StealthError::HostInvocation(format!("failed to spawn powershell.exe: {}", e))
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes()).await.map_err(|e| {
            StealthError::HostInvocation(format!("failed to write script to stdin: {}", e))
        })?;
        // Dropping stdin closes the pipe, signalling EOF.
        drop(stdin);
    }

    let output = match timeout(ceiling, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Err(StealthError::HostInvocation(format!(
                "powershell.exe wait failed: {}",
                e
            )))
        }
        Err(_) => {
            return Err(StealthError::PowerShellFailure {
                exit_code: -1,
                stderr: format!("powershell.exe timed out after {:?}", ceiling),
            })
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        return Err(StealthError::PowerShellFailure {
            exit_code: output.status.code().unwrap_or(-1),
            stderr: if stderr.is_empty() { stdout } else { stderr },
        });
    }

    Ok(stdout)
}

/// Convenience wrapper for transfers, which need the longer ceiling.
pub(crate) async fn run_host_ps_transfer(script: &str) -> Result<String, StealthError> {
    run_host_ps_with_timeout(script, HOST_PS_TRANSFER_TIMEOUT).await
}

// ---------------------------------------------------------------------------
// Script transport into the guest
// ---------------------------------------------------------------------------

/// Wrap a guest script in a host-side invocation of PowerShell Direct.
///
/// The guest script is base64-encoded and decoded inside the guest. Base64 is
/// drawn from `A–Z a–z 0–9 + / =`, so it embeds inside a single-quoted
/// PowerShell literal with no escaping at all — no here-strings, no doubled
/// quotes, no length ceiling on the payload.
///
/// The rendered script throws (`$ErrorActionPreference = 'Stop'`) on any guest
/// failure, so [`run_host_ps`] reports it as a non-zero exit.
pub(crate) fn build_guest_invocation(
    vm_name: &str,
    credentials: &GuestCredentials,
    guest_script: &str,
) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(guest_script.as_bytes());

    format!(
        r#"$ErrorActionPreference = 'Stop'

$bytes = [Convert]::FromBase64String({payload})
$text  = [Text.Encoding]::UTF8.GetString($bytes)
$block = [ScriptBlock]::Create($text)

$user  = {user}
$pass  = ConvertTo-SecureString {pass} -AsPlainText -Force
$cred  = New-Object System.Management.Automation.PSCredential($user, $pass)

Invoke-Command -VMName {vm} -Credential $cred -ScriptBlock $block -ErrorAction Stop | Out-String
"#,
        payload = ps_literal(&encoded),
        user = ps_literal(credentials.username()),
        pass = ps_literal(credentials.password()),
        vm = ps_literal(vm_name),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_machine_qualified() {
        let creds = GuestCredentials::new("Administrator", "pw");
        assert_eq!(creds.username(), r".\Administrator");
    }

    #[test]
    fn qualified_username_is_left_alone() {
        let creds = GuestCredentials::new(r"LAB\bob", "pw");
        assert_eq!(creds.username(), r"LAB\bob");
    }

    #[test]
    fn invocation_round_trips_the_script() {
        let creds = GuestCredentials::default();
        let script = "Write-Output \"it's $tricky\"";
        let rendered = build_guest_invocation("TestVM", &creds, script);

        // Extract the base64 payload and confirm it decodes back verbatim.
        let start = rendered.find("FromBase64String('").unwrap() + "FromBase64String('".len();
        let end = start + rendered[start..].find('\'').unwrap();
        let encoded = &rendered[start..end];

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("payload must be valid base64");
        assert_eq!(String::from_utf8(decoded).unwrap(), script);
    }

    #[test]
    fn invocation_targets_the_vm_and_escapes_the_password() {
        let creds = GuestCredentials::new("Administrator", "it's-a-pw");
        let rendered = build_guest_invocation("Alpha", &creds, "Get-Date");

        assert!(rendered.contains("-VMName 'Alpha'"));
        assert!(rendered.contains("'it''s-a-pw'"));
        assert!(rendered.contains("-ErrorAction Stop"));
    }
}