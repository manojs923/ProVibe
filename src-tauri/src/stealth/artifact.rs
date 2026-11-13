use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, error, info, instrument, warn};

use crate::hyper_v::vm_manager::{VmManager, VmManagerOps};
use crate::stealth::StealthError;
use crate::util::powershell::PowerShellScript;
use crate::vm::config::StealthConfig;
use crate::vm::lifecycle::VmHandle;

const BACKUP_SUFFIX: &str = ".stealth_bak";
const ARTIFACT_SCAN_TIMEOUT_SECS: u64 = 300;
/// Fallback restore root when the config doesn't specify one.
const DEFAULT_RESTORE_ROOT: &str = r"C:\Restored\";

#[derive(Debug, Error)]
pub enum GuestArtifactError {
    #[error("Failed to execute PowerShell in guest: {0}")]
    PowerShellExecution(String),

    #[error("VM '{0}' is not in a running state")]
    VmNotRunning(String),

    #[error("Artifact scan timed out after {timeout_secs} seconds")]
    ScanTimeout { timeout_secs: u64 },

    #[error("Failed to backup artifact at '{0}': {1}")]
    BackupFailure(String, String),

    #[error("Failed to restore artifact from '{0}': {1}")]
    RestoreFailure(String, String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<GuestArtifactError> for StealthError {
    fn from(err: GuestArtifactError) -> Self {
        StealthError::GuestArtifact(err)
    }
}

impl From<StealthError> for GuestArtifactError {
    fn from(err: StealthError) -> Self {
        GuestArtifactError::PowerShellExecution(err.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct GuestArtifact {
    pub name: String,
    pub guest_path: PathBuf,
    pub host_backup_path: PathBuf,
    pub size_bytes: u64,
    pub is_directory: bool,
}

/// Outcome of a full cleanup or restore sweep. Non-fatal per-item failures are
/// collected so one bad artifact can't abort the run.
#[derive(Debug, Clone, Default)]
pub struct CleanupReport {
    pub discovered: usize,
    pub cleaned: usize,
    pub restored: usize,
    pub failed: Vec<(String, String)>,
}

impl CleanupReport {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }
}

pub struct GuestArtifactCleaner {
    config: StealthConfig,
    vm_manager: Box<dyn VmManagerOps>,
}

impl GuestArtifactCleaner {
    #[instrument(name = "guest_artifact_cleaner_new", skip(config, vm_manager))]
    pub fn new<M>(config: StealthConfig, vm_manager: M) -> Self
    where
        M: VmManagerOps + 'static,
    {
        Self {
            config,
            vm_manager: Box::new(vm_manager),
        }
    }

    /// Build a cleaner from a VM handle alone, using default paths. This is
    /// what the tunnel/stealth layer uses, since it only carries a handle.
    pub fn for_vm(vm: &VmHandle) -> Self {
        let config = StealthConfig {
            guest_vm_name: vm.name.clone(),
            ..Default::default()
        };
        Self::new(config, VmManager::new())
    }

    /// Scans the guest VM for known artifacts left by prior tooling or scripts.
    /// Returns a list of discovered artifacts that can be cleaned or backed up.
    #[instrument(name = "scan_for_artifacts", skip(self))]
    pub async fn scan_for_artifacts(&self) -> Result<Vec<GuestArtifact>, GuestArtifactError> {
        let vm_name = &self.config.guest_vm_name;

        if !self.vm_manager.is_vm_running(vm_name).await? {
            return Err(GuestArtifactError::VmNotRunning(vm_name.clone()));
        }

        // Runs inside the guest via Hyper-V Guest Services. Requires the Guest
        // Services integration component to be enabled and running.
        let scan_script = PowerShellScript::new(
            r#"
$ErrorActionPreference = "Stop"

# Known artifact locations left by tooling
$artifactPaths = @(
    "C:\\ProgramData\\StealthTool\\Logs",
    "C:\\ProgramData\\StealthTool\\Temp",
    "C:\\Windows\\Temp\\stealth_*.tmp",
    "C:\\Users\\Public\\Documents\\stealth_*.json",
    "$env:TEMP\\stealth_*"
)

$discovered = @()

foreach ($path in $artifactPaths) {
    if (Test-Path $path -PathType Any) {
        $items = Get-Item $path -Force
        foreach ($item in $items) {
            $discovered += [PSCustomObject]@{
                Name = $item.Name
                FullName = $item.FullName
                SizeBytes = if ($item.PSIsContainer) { 0 } else { $item.Length }
                IsDirectory = $item.PSIsContainer
                LastWriteTime = $item.LastWriteTime.ToString("o")
            }
        }
    }
}

# Force array shape even for a single hit, so the host parser has one case.
@($discovered) | ConvertTo-Json -Compress -Depth 4
"#,
        );

        let output = tokio::time::timeout(
            Duration::from_secs(ARTIFACT_SCAN_TIMEOUT_SECS),
            self.vm_manager.execute_in_guest(vm_name, scan_script),
        )
        .await
        .map_err(|_| GuestArtifactError::ScanTimeout {
            timeout_secs: ARTIFACT_SCAN_TIMEOUT_SECS,
        })??;

        let artifacts = self.parse_scan_output(&output)?;
        debug!(
            "Discovered {} artifacts in guest VM '{}'",
            artifacts.len(),
            vm_name
        );
        Ok(artifacts)
    }

    /// Creates a backup of the specified artifact on the host filesystem before deletion.
    #[instrument(name = "backup_artifact", skip(self))]
    pub async fn backup_artifact(&self, artifact: &GuestArtifact) -> Result<(), GuestArtifactError> {
        let backup_dir = &self.config.backup_directory;

        fs::create_dir_all(backup_dir).map_err(|e| {
            GuestArtifactError::BackupFailure(backup_dir.display().to_string(), e.to_string())
        })?;

        let guest_src = artifact.guest_path.to_string_lossy().to_string();
        let host_dst = artifact.host_backup_path.to_string_lossy().to_string();

        self.vm_manager
            .copy_file_from_guest(&self.config.guest_vm_name, &guest_src, &host_dst)
            .await
            .map_err(|e| GuestArtifactError::BackupFailure(guest_src.clone(), e.to_string()))?;

        info!(
            "Backed up '{}' from guest to '{}' on host",
            guest_src, host_dst
        );
        Ok(())
    }

    /// Backs up and then removes the artifact from the guest VM.
    #[instrument(name = "clean_artifact", skip(self))]
    pub async fn clean_artifact(&self, artifact: &GuestArtifact) -> Result<(), GuestArtifactError> {
        self.backup_artifact(artifact).await?;

        // Single-quote escaping for the guest-side path literal.
        let guest_path_str = artifact.guest_path.to_string_lossy().replace('\'', "''");

        let cleanup_script = PowerShellScript::new(format!(
            r#"
$ErrorActionPreference = "Stop"

$targetPath = '{path}'

if (Test-Path $targetPath) {{
    if ((Get-Item $targetPath).PSIsContainer) {{
        Remove-Item -Path $targetPath -Recurse -Force
    }} else {{
        Remove-Item -Path $targetPath -Force
    }}
    Write-Output "REMOVED:$targetPath"
}} else {{
    Write-Output "NOTFOUND:$targetPath"
}}
"#,
            path = guest_path_str
        ));

        let output = self
            .vm_manager
            .execute_in_guest(&self.config.guest_vm_name, cleanup_script)
            .await
            .map_err(|e| GuestArtifactError::PowerShellExecution(e.to_string()))?;

        if output.contains("NOTFOUND") {
            warn!(
                "Artifact '{}' was not found in guest during cleanup (already removed?)",
                artifact.guest_path.display()
            );
        } else if output.contains("REMOVED") {
            info!(
                "Successfully removed artifact '{}' from guest",
                artifact.guest_path.display()
            );
        }

        Ok(())
    }

    /// Restores a previously backed-up artifact from the host back into the guest VM.
    #[instrument(name = "restore_artifact", skip(self))]
    pub async fn restore_artifact(&self, artifact: &GuestArtifact) -> Result<(), GuestArtifactError> {
        if !artifact.host_backup_path.exists() {
            return Err(GuestArtifactError::RestoreFailure(
                artifact.host_backup_path.display().to_string(),
                "Backup file does not exist on host".to_string(),
            ));
        }

        let host_src = artifact.host_backup_path.to_string_lossy().to_string();
        let guest_dst = artifact.guest_path.to_string_lossy().to_string();

        self.vm_manager
            .copy_file_to_guest(&self.config.guest_vm_name, &host_src, &guest_dst)
            .await
            .map_err(|e| {
                GuestArtifactError::RestoreFailure(
                    host_src.clone(),
                    format!("Failed copy host->guest: {}", e),
                )
            })?;

        info!("Restored '{}' to guest path '{}'", host_src, guest_dst);
        Ok(())
    }

    /// Full cleanup cycle: scan, backup, and remove all discovered artifacts.
    #[instrument(name = "full_cleanup", skip(self))]
    pub async fn full_cleanup(&self) -> Result<CleanupReport, GuestArtifactError> {
        let artifacts = self.scan_for_artifacts().await?;
        let mut report = CleanupReport {
            discovered: artifacts.len(),
            ..Default::default()
        };

        for artifact in &artifacts {
            match self.clean_artifact(artifact).await {
                Ok(()) => report.cleaned += 1,
                Err(e) => {
                    error!(
                        "Failed to clean artifact '{}': {}",
                        artifact.guest_path.display(),
                        e
                    );
                    report
                        .failed
                        .push((artifact.guest_path.display().to_string(), e.to_string()));
                }
            }
        }

        Ok(report)
    }

    /// Full restore cycle: restores all artifacts that have backups on the host.
    #[instrument(name = "full_restore", skip(self))]
    pub async fn full_restore(&self) -> Result<CleanupReport, GuestArtifactError> {
        let backup_dir = &self.config.backup_directory;
        let mut report = CleanupReport::default();

        if !backup_dir.exists() {
            debug!(
                "No backup directory found at '{}', nothing to restore",
                backup_dir.display()
            );
            return Ok(report);
        }

        for entry in fs::read_dir(backup_dir)? {
            let entry = entry?;
            let path = entry.path();

            if !path.to_string_lossy().contains(BACKUP_SUFFIX) {
                continue;
            }

            let artifact = GuestArtifact {
                name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                guest_path: self.derive_guest_path(&path)?,
                host_backup_path: path.clone(),
                size_bytes: entry.metadata()?.len(),
                is_directory: path.is_dir(),
            };

            match self.restore_artifact(&artifact).await {
                Ok(()) => report.restored += 1,
                Err(e) => {
                    error!("Failed to restore artifact from '{}': {}", path.display(), e);
                    report.failed.push((path.display().to_string(), e.to_string()));
                }
            }
        }

        info!("Restored {} artifacts to guest VM", report.restored);
        Ok(report)
    }

    fn parse_scan_output(&self, raw_output: &str) -> Result<Vec<GuestArtifact>, GuestArtifactError> {
        let trimmed = raw_output.trim();
        if trimmed.is_empty() || trimmed == "null" {
            return Ok(Vec::new());
        }

        #[derive(serde::Deserialize)]
        struct ScannedItem {
            #[serde(rename = "Name")]
            name: String,
            #[serde(rename = "FullName")]
            full_name: String,
            #[serde(rename = "SizeBytes", default)]
            size_bytes: u64,
            #[serde(rename = "IsDirectory", default)]
            is_directory: bool,
        }

        let parse_err =
            |e: serde_json::Error| GuestArtifactError::PowerShellExecution(format!(
                "Failed to parse scan output JSON: {}",
                e
            ));

        // ConvertTo-Json emits a bare object for a single item even when the
        // pipeline input was an array, so accept both shapes.
        let value: serde_json::Value =
            serde_json::from_str(trimmed).map_err(parse_err)?;

        let items: Vec<ScannedItem> = match value {
            serde_json::Value::Array(_) => serde_json::from_value(value).map_err(parse_err)?,
            serde_json::Value::Object(_) => {
                vec![serde_json::from_value(value).map_err(parse_err)?]
            }
            serde_json::Value::Null => Vec::new(),
            _ => Vec::new(),
        };

        let backup_dir = &self.config.backup_directory;

        Ok(items
            .into_iter()
            .map(|item| {
                let host_backup_path = backup_dir.join(format!("{}{}", item.name, BACKUP_SUFFIX));
                GuestArtifact {
                    name: item.name,
                    guest_path: PathBuf::from(&item.full_name),
                    host_backup_path,
                    size_bytes: item.size_bytes,
                    is_directory: item.is_directory,
                }
            })
            .collect())
    }

    fn derive_guest_path(&self, backup_host_path: &Path) -> Result<PathBuf, GuestArtifactError> {
        // Backup filenames follow the pattern: <original_name>.stealth_bak
        let file_name = backup_host_path
            .file_name()
            .ok_or_else(|| {
                GuestArtifactError::RestoreFailure(
                    backup_host_path.display().to_string(),
                    "Could not extract filename from backup path".to_string(),
                )
            })?
            .to_string_lossy();

        let original_name = file_name
            .strip_suffix(BACKUP_SUFFIX)
            .unwrap_or(file_name.as_ref());

        // Full original paths aren't preserved in the backup name; a manifest
        // would be needed to restore to the exact source directory.
        let guest_restore_root = self
            .config
            .guest_restore_root
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_RESTORE_ROOT));

        Ok(guest_restore_root.join(original_name))
    }
}

// ---------------------------------------------------------------------------
// Free-function entry points used by the stealth layer
// ---------------------------------------------------------------------------

/// Scan, back up, and remove every known guest artifact.
pub async fn run_full_cleanup(vm: &VmHandle) -> Result<CleanupReport, GuestArtifactError> {
    GuestArtifactCleaner::for_vm(vm).full_cleanup().await
}

/// Restore every backed-up artifact into the guest.
pub async fn run_full_restore(vm: &VmHandle) -> Result<CleanupReport, GuestArtifactError> {
    GuestArtifactCleaner::for_vm(vm).full_restore().await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hyper_v::vm_manager::MockVmManager;
    use mockall::predicate::*;

    fn test_config() -> StealthConfig {
        StealthConfig {
            guest_vm_name: "TestVM".to_string(),
            backup_directory: PathBuf::from("/tmp/stealth_backups"),
            guest_restore_root: Some(PathBuf::from(r"C:\Restored\")),
            ..Default::default()
        }
    }

    fn artifact(name: &str) -> GuestArtifact {
        GuestArtifact {
            name: name.to_string(),
            guest_path: PathBuf::from(format!(r"C:\ProgramData\StealthTool\Logs\{}", name)),
            host_backup_path: PathBuf::from(format!("/tmp/stealth_backups/{}{}", name, BACKUP_SUFFIX)),
            size_bytes: 1024,
            is_directory: false,
        }
    }

    #[tokio::test]
    async fn scan_parses_output_correctly() {
        let mut mock_vm = MockVmManager::new();
        let json_output = r#"[{"Name":"test.log","FullName":"C:\\ProgramData\\StealthTool\\Logs\\test.log","SizeBytes":1024,"IsDirectory":false}]"#;

        mock_vm
            .expect_is_vm_running()
            .with(eq("TestVM"))
            .times(1)
            .returning(|_| Box::pin(async { Ok(true) }));

        mock_vm
            .expect_execute_in_guest()
            .times(1)
            .returning(move |_, _| Box::pin(async move { Ok(json_output.to_string()) }));

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        let result = cleaner.scan_for_artifacts().await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "test.log");
        assert_eq!(
            result[0].guest_path,
            PathBuf::from(r"C:\ProgramData\StealthTool\Logs\test.log")
        );
        assert_eq!(result[0].size_bytes, 1024);
        assert!(!result[0].is_directory);
    }

    #[tokio::test]
    async fn scan_errors_when_vm_not_running() {
        let mut mock_vm = MockVmManager::new();

        mock_vm
            .expect_is_vm_running()
            .with(eq("TestVM"))
            .times(1)
            .returning(|_| Box::pin(async { Ok(false) }));

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        let result = cleaner.scan_for_artifacts().await;

        match result.unwrap_err() {
            GuestArtifactError::VmNotRunning(name) => assert_eq!(name, "TestVM"),
            other => panic!("expected VmNotRunning, got {:?}", other),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn scan_times_out() {
        let mut mock_vm = MockVmManager::new();

        mock_vm
            .expect_is_vm_running()
            .times(1)
            .returning(|_| Box::pin(async { Ok(true) }));

        mock_vm.expect_execute_in_guest().times(1).returning(|_, _| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(ARTIFACT_SCAN_TIMEOUT_SECS + 10)).await;
                Ok(String::new())
            })
        });

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        let result = cleaner.scan_for_artifacts().await;

        match result.unwrap_err() {
            GuestArtifactError::ScanTimeout { timeout_secs } => {
                assert_eq!(timeout_secs, ARTIFACT_SCAN_TIMEOUT_SECS)
            }
            other => panic!("expected ScanTimeout, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn backup_artifact_succeeds() {
        let mut mock_vm = MockVmManager::new();

        mock_vm
            .expect_copy_file_from_guest()
            .with(
                eq("TestVM"),
                eq(r"C:\ProgramData\StealthTool\Logs\test.log"),
                eq("/tmp/stealth_backups/test.log.stealth_bak"),
            )
            .times(1)
            .returning(|_, _, _| Box::pin(async { Ok(()) }));

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        cleaner.backup_artifact(&artifact("test.log")).await.unwrap();
    }

    #[tokio::test]
    async fn restore_errors_when_backup_missing() {
        let mock_vm = MockVmManager::new();

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        let result = cleaner.restore_artifact(&artifact("nonexistent.log")).await;

        match result.unwrap_err() {
            GuestArtifactError::RestoreFailure(_, msg) => {
                assert!(msg.contains("does not exist"));
            }
            other => panic!("expected RestoreFailure, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn clean_artifact_removes_file() {
        let mut mock_vm = MockVmManager::new();

        mock_vm
            .expect_copy_file_from_guest()
            .times(1)
            .returning(|_, _, _| Box::pin(async { Ok(()) }));

        mock_vm
            .expect_execute_in_guest()
            .with(eq("TestVM"), always())
            .times(1)
            .returning(|_, _| {
                Box::pin(async { Ok(r"REMOVED:C:\ProgramData\StealthTool\Logs\test.log".to_string()) })
            });

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        cleaner.clean_artifact(&artifact("test.log")).await.unwrap();
    }

    #[tokio::test]
    async fn clean_artifact_tolerates_already_removed() {
        let mut mock_vm = MockVmManager::new();

        mock_vm
            .expect_copy_file_from_guest()
            .times(1)
            .returning(|_, _, _| Box::pin(async { Ok(()) }));

        mock_vm
            .expect_execute_in_guest()
            .with(eq("TestVM"), always())
            .times(1)
            .returning(|_, _| Box::pin(async { Ok("NOTFOUND:gone".to_string()) }));

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        cleaner.clean_artifact(&artifact("missing.log")).await.unwrap();
    }

    #[tokio::test]
    async fn full_cleanup_processes_all_artifacts() {
        let mut mock_vm = MockVmManager::new();

        mock_vm
            .expect_is_vm_running()
            .times(1)
            .returning(|_| Box::pin(async { Ok(true) }));

        let json_output = r#"[
            {"Name":"a.log","FullName":"C:\\ProgramData\\StealthTool\\Logs\\a.log","SizeBytes":100,"IsDirectory":false},
            {"Name":"b.log","FullName":"C:\\ProgramData\\StealthTool\\Logs\\b.log","SizeBytes":200,"IsDirectory":false}
        ]"#;

        mock_vm
            .expect_execute_in_guest()
            .times(3)
            .returning(move |_, script| {
                let is_scan = script.as_string().contains("artifactPaths");
                Box::pin(async move {
                    if is_scan {
                        Ok(json_output.to_string())
                    } else {
                        Ok("REMOVED:ok".to_string())
                    }
                })
            });

        mock_vm
            .expect_copy_file_from_guest()
            .times(2)
            .returning(|_, _, _| Box::pin(async { Ok(()) }));

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        let report = cleaner.full_cleanup().await.unwrap();

        assert_eq!(report.discovered, 2);
        assert_eq!(report.cleaned, 2);
        assert!(report.is_clean());
    }

    #[tokio::test]
    async fn full_cleanup_continues_after_individual_failure() {
        let mut mock_vm = MockVmManager::new();

        mock_vm
            .expect_is_vm_running()
            .times(1)
            .returning(|_| Box::pin(async { Ok(true) }));

        let json_output = r#"[
            {"Name":"good.log","FullName":"C:\\ProgramData\\StealthTool\\Logs\\good.log","SizeBytes":50,"IsDirectory":false},
            {"Name":"bad.log","FullName":"C:\\ProgramData\\StealthTool\\Logs\\bad.log","SizeBytes":50,"IsDirectory":false}
        ]"#;

        mock_vm
            .expect_execute_in_guest()
            .times(3)
            .returning(move |_, script| {
                let s = script.as_string().to_string();
                let is_scan = s.contains("artifactPaths");
                let is_bad = s.contains("bad.log");
                Box::pin(async move {
                    if is_scan {
                        Ok(json_output.to_string())
                    } else if is_bad {
                        Err(StealthError::GuestServices("unavailable".into()))
                    } else {
                        Ok("REMOVED:ok".to_string())
                    }
                })
            });

        mock_vm
            .expect_copy_file_from_guest()
            .times(2)
            .returning(|_, _, _| Box::pin(async { Ok(()) }));

        let cleaner = GuestArtifactCleaner::new(test_config(), mock_vm);
        let report = cleaner.full_cleanup().await.unwrap();

        // Both discovered, one cleaned, one recorded as failed.
        assert_eq!(report.discovered, 2);
        assert_eq!(report.cleaned, 1);
        assert_eq!(report.failed.len(), 1);
    }

    #[tokio::test]
    async fn full_restore_returns_empty_when_no_backup_dir() {
        let mock_vm = MockVmManager::new();
        let mut config = test_config();
        config.backup_directory = PathBuf::from("/tmp/definitely_does_not_exist_dir");

        let cleaner = GuestArtifactCleaner::new(config, mock_vm);
        let report = cleaner.full_restore().await.unwrap();

        assert_eq!(report.restored, 0);
    }

    #[tokio::test]
    async fn parse_scan_output_handles_empty() {
        let cleaner = GuestArtifactCleaner::new(test_config(), MockVmManager::new());
        assert!(cleaner.parse_scan_output("").unwrap().is_empty());
        assert!(cleaner.parse_scan_output("null").unwrap().is_empty());
    }

    #[tokio::test]
    async fn parse_scan_output_handles_single_object() {
        let cleaner = GuestArtifactCleaner::new(test_config(), MockVmManager::new());

        // ConvertTo-Json emits a bare object for one item.
        let single_json =
            r#"{"Name":"only.log","FullName":"C:\\Temp\\only.log","SizeBytes":42,"IsDirectory":false}"#;

        let parsed = cleaner.parse_scan_output(single_json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "only.log");
    }

    #[tokio::test]
    async fn parse_scan_output_handles_missing_optional_fields() {
        let cleaner = GuestArtifactCleaner::new(test_config(), MockVmManager::new());

        let json = r#"[{"Name":"x.log","FullName":"C:\\Temp\\x.log"}]"#;
        let parsed = cleaner.parse_scan_output(json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].size_bytes, 0);
        assert!(!parsed[0].is_directory);
    }

    #[tokio::test]
    async fn derive_guest_path_strips_suffix() {
        let cleaner = GuestArtifactCleaner::new(test_config(), MockVmManager::new());

        let guest_path = cleaner
            .derive_guest_path(Path::new("/tmp/stealth_backups/myfile.log.stealth_bak"))
            .unwrap();

        assert_eq!(guest_path, PathBuf::from(r"C:\Restored\myfile.log"));
    }

    #[tokio::test]
    async fn backup_creates_directory() {
        let mut mock_vm = MockVmManager::new();

        mock_vm
            .expect_copy_file_from_guest()
            .times(1)
            .returning(|_, _, _| Box::pin(async { Ok(()) }));

        let dir = std::env::temp_dir().join("stealth_test_backup_creation");
        let mut config = test_config();
        config.backup_directory = dir.clone();

        let mut a = artifact("dir_test.log");
        a.host_backup_path = dir.join("dir_test.log.stealth_bak");

        let cleaner = GuestArtifactCleaner::new(config, mock_vm);
        cleaner.backup_artifact(&a).await.unwrap();

        assert!(dir.exists());
    }

    #[test]
    fn guest_artifact_error_display() {
        assert_eq!(
            GuestArtifactError::VmNotRunning("MyVM".to_string()).to_string(),
            "VM 'MyVM' is not in a running state"
        );
        assert_eq!(
            GuestArtifactError::ScanTimeout { timeout_secs: 300 }.to_string(),
            "Artifact scan timed out after 300 seconds"
        );
        assert_eq!(
            GuestArtifactError::BackupFailure("/p".into(), "disk full".into()).to_string(),
            "Failed to backup artifact at '/p': disk full"
        );
    }

    #[test]
    fn guest_artifact_error_converts_to_stealth_error() {
        let err: StealthError = GuestArtifactError::VmNotRunning("VM".into()).into();
        assert!(matches!(err, StealthError::GuestArtifact(_)));
    }
}