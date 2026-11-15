//! src-tauri/src/hyper_v/vm_manager.rs
//!
//! Guest interaction surface.
//!
//! Everything the artifact cleaner needs from a VM goes through
//! [`VmManagerOps`]: liveness, in-guest execution, and file movement. The trait
//! exists so the cleaner can be driven by `MockVmManager` in tests without a
//! hypervisor.
//!
//! Methods return boxed futures rather than being plain `async fn` because the
//! cleaner stores `Box<dyn VmManagerOps>` — native async-in-trait is not
//! object-safe.

use async_trait::async_trait;

use crate::stealth::StealthError;
use crate::util::ps_literal;
use crate::util::powershell::PowerShellScript;

use super::{build_guest_invocation, run_host_ps, run_host_ps_transfer, GuestCredentials};

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// Host-side operations against a single guest VM.
///
/// Every method takes the VM name explicitly rather than being bound to one
/// VM, so a single manager can serve several guests.
#[async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait VmManagerOps: Send + Sync {
    /// Whether the named VM exists and is currently running.
    ///
    /// A missing VM is `Ok(false)`, not an error — callers distinguish
    /// "absent" from "the host is broken".
    async fn is_vm_running(&self, vm_name: &str) -> Result<bool, StealthError>;

    /// Execute a PowerShell script inside the guest and return its output.
    ///
    /// Requires PowerShell Direct, i.e. the guest must be running and the
    /// supplied credentials must be local administrators.
    async fn execute_in_guest(
        &self,
        vm_name: &str,
        script: PowerShellScript,
    ) -> Result<String, StealthError>;

    /// Copy a file out of the guest onto the host.
    async fn copy_file_from_guest(
        &self,
        vm_name: &str,
        guest_path: &str,
        host_path: &str,
    ) -> Result<(), StealthError>;

    /// Copy a file from the host into the guest.
    async fn copy_file_to_guest(
        &self,
        vm_name: &str,
        host_path: &str,
        guest_path: &str,
    ) -> Result<(), StealthError>;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Real manager, backed by the Hyper-V PowerShell surface.
///
/// File movement uses `Copy-VMFile`, which depends on the Guest Service
/// Interface integration component being enabled in the guest. Script execution
/// does not — it uses PowerShell Direct instead.
#[derive(Debug, Clone, Default)]
pub struct VmManager {
    credentials: GuestCredentials,
}

impl VmManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use explicit guest credentials instead of the image defaults.
    pub fn with_credentials(mut self, credentials: GuestCredentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// Fail early with a specific error when the guest cannot take a
    /// PowerShell Direct session, rather than letting `Invoke-Command`
    /// produce an opaque transport fault.
    async fn require_running(&self, vm_name: &str) -> Result<(), StealthError> {
        if self.is_vm_running(vm_name).await? {
            Ok(())
        } else {
            Err(StealthError::InvalidHandle(format!(
                "VM '{}' is not running",
                vm_name
            )))
        }
    }
}

#[async_trait]
impl VmManagerOps for VmManager {
    async fn is_vm_running(&self, vm_name: &str) -> Result<bool, StealthError> {
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$vm = Get-VM -Name {vm} -ErrorAction SilentlyContinue
if ($null -eq $vm) {{ 'NotFound' }} else {{ $vm.State.ToString() }}
"#,
            vm = ps_literal(vm_name)
        );

        let state = run_host_ps(&script).await?;
        Ok(state.eq_ignore_ascii_case("Running"))
    }

    async fn execute_in_guest(
        &self,
        vm_name: &str,
        script: PowerShellScript,
    ) -> Result<String, StealthError> {
        self.require_running(vm_name).await?;

        let invocation = build_guest_invocation(vm_name, &self.credentials, script.as_string());
        run_host_ps(&invocation).await
    }

    async fn copy_file_from_guest(
        &self,
        vm_name: &str,
        guest_path: &str,
        host_path: &str,
    ) -> Result<(), StealthError> {
        self.require_running(vm_name).await?;

        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
Copy-VMFile -VMName {vm} -SourcePath {src} -DestinationPath {dst} `
    -FileSource Guest -Force -CreateFullPath -ErrorAction Stop
"#,
            vm = ps_literal(vm_name),
            src = ps_literal(guest_path),
            dst = ps_literal(host_path),
        );

        run_host_ps_transfer(&script).await.map(|_| ())
    }

    async fn copy_file_to_guest(
        &self,
        vm_name: &str,
        host_path: &str,
        guest_path: &str,
    ) -> Result<(), StealthError> {
        self.require_running(vm_name).await?;

        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
Copy-VMFile -VMName {vm} -SourcePath {src} -DestinationPath {dst} `
    -FileSource Host -Force -CreateFullPath -ErrorAction Stop
"#,
            vm = ps_literal(vm_name),
            src = ps_literal(host_path),
            dst = ps_literal(guest_path),
        );

        run_host_ps_transfer(&script).await.map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_defaults_to_image_credentials() {
        let manager = VmManager::new();
        assert_eq!(manager.credentials.username(), r".\Administrator");
    }

    #[test]
    fn credentials_are_overridable() {
        let manager = VmManager::new().with_credentials(GuestCredentials::new("bob", "pw"));
        assert_eq!(manager.credentials.username(), r".\bob");
    }

    #[tokio::test]
    async fn mock_satisfies_the_object_safe_trait() {
        let mut mock = MockVmManager::new();
        mock.expect_is_vm_running()
            .returning(|_| Box::pin(async { Ok(true) }));

        let boxed: Box<dyn VmManagerOps> = Box::new(mock);
        assert!(boxed.is_vm_running("Anything").await.unwrap());
    }
}