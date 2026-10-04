use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;

#[derive(Debug, Clone)]
pub struct CloneResult {
    pub disk_path: PathBuf,
    pub parent_path: PathBuf,
    pub size_bytes: u64,
}

/// Validates that the base VHDX exists and is readable.
/// If the golden image is missing, everything downstream dies,
/// so we check before wasting cycles.
pub fn validate_base_image(base_path: &Path) -> Result<(), String> {
    if !base_path.exists() {
        return Err(format!(
            "Base VHDX not found at {}. Build or place your golden image first.",
            base_path.display()
        ));
    }

    let metadata = fs::metadata(base_path)
        .map_err(|e| format!("Cannot read base VHDX metadata: {}", e))?;

    if metadata.len() < 1024 * 1024 {
        return Err("Base VHDX appears corrupted (under 1MB). Rebuild the golden image.".to_string());
    }

    // Verify VHDX magic bytes (0x76686478 = "vhdx")
    // We don't need to be fancy, just confirm it's actually a VHDX and not a random file renamed.
    let mut header = [0u8; 8];
    let file = fs::File::open(base_path)
        .map_err(|e| format!("Cannot open base VHDX: {}", e))?;
    use std::io::Read;
    file.take(8).read(&mut header)
        .map_err(|e| format!("Cannot read VHDX header: {}", e))?;

    if &header[0..4] != b"vhdx" {
        return Err("File does not have VHDX magic bytes. Is this actually a VHDX?".to_string());
    }

    Ok(())
}

/// Creates a differencing disk from the base golden image.
/// The differencing disk starts at ~4MB and grows only as the VM writes changes.
/// This is what makes spin-up near-instant vs copying a 40GB+ fixed disk.
pub fn create_differencing_disk(
    vm_name: &str,
    base_path: &Path,
    output_dir: &Path,
) -> Result<CloneResult, String> {
    validate_base_image(base_path)?;

    fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create VM disk directory: {}", e))?;

    let disk_path = output_dir.join(format!("{}_diff.vhdx", vm_name);

    // If a stale differencing disk exists from a previous run, nuke it.
    // We don't want the new session inheriting old writes.
    if disk_path.exists() {
        fs::remove_file(&disk_path)
            .map_err(|e| format!("Failed to remove stale differencing disk: {}", e))?;
    }

    let output_str = disk_path.to_string_lossy().to_string();
    let base_str = base_path.to_string_lossy().to_string();

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "New-VHD -Path '{}' -ParentPath '{}' -Differencing | Select-Object -ExpandProperty Path",
                output_str, base_str
            ),
        ])
        .output()
        .map_err(|e| format!("Failed to spawn PowerShell for VHDX creation: {}", e))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(format!(
            "New-VHD differencing disk creation failed. Ensure Hyper-V module is installed. stderr: {}",
            stderr
        ));
    }

    let size = fs::metadata(&disk_path)
        .map_err(|e| format!("Differencing disk created but cannot read its size: {}", e))?
        .len();

    Ok(CloneResult {
        disk_path,
        parent_path: base_path.to_path_buf(),
        size_bytes: size,
    })
}

/// Destroys the differencing disk. The base image is never touched.
/// Only the delta writes from this specific session are obliterated.
/// After this call, there is zero forensic residue from the session's disk activity.
pub fn destroy_differencing_disk(disk_path: &Path) -> Result<(), String> {
    if !disk_path.exists() {
        return Ok(()); // Already gone, we don't care.
    }

    // First attempt: PowerShell Remove-VHD (handles attached disk scenarios cleanly)
    let path_str = disk_path.to_string_lossy().to_string();
    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("Remove-VHD -Path '{}' -Force", path_str),
        ])
        .output()
        .map_err(|e| format!("Failed to spawn PowerShell for VHDX destruction: {}", e))?;

    if !status.status.success() {
        // Fallback: raw file deletion if the disk was already detached
        fs::remove_file(disk_path)
            .map_err(|e| {
                format!(
                    "Failed to destroy differencing disk (PS failed, raw delete failed): {}",
                    e
                )
            })?;
    }

    Ok(())
}

/// Gets the current size of the differencing disk.
/// Useful for monitoring how much the VM is writing (and how much evidence is accumulating).
pub fn get_disk_usage(disk_path: &Path) -> Result<u64, String> {
    let metadata = fs::metadata(disk_path)
        .map_err(|e| format!("Cannot stat differencing disk: {}", e))?;
    Ok(metadata.len())
}

/// Enumerates all differencing disks in the output directory.
/// Useful for a cleanup sweep if a previous session crashed without proper teardown.
pub fn find_orphaned_disks(output_dir: &Path) -> Vec<PathBuf> {
    let mut orphans = Vec::new();

    if let Ok(entries) = fs::read_dir(output_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "vhdx" && !path.to_string_lossy().contains("base") {
                    orphans.push(path);
                }
            }
        }
    }

    orphans
}

/// Nukes every orphaned differencing disk found. Call this on application startup
/// to ensure you never carry state from a crashed previous session.
pub fn purge_all_orphans(output_dir: &Path) -> Result<usize, String> {
    let orphans = find_orphaned_disks(output_dir);
    let mut purged = 0;

    for disk in orphans {
        match destroy_differencing_disk(&disk) {
            Ok(()) => purged += 1,
            Err(e) => return Err(format!("Orphan purge incomplete at {}: {}", disk.display(), e)),
        }
    }

    Ok(purged)
}