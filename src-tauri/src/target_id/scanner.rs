use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, MAX_PATH};
use windows_sys::Win32::Security::Cryptography::{
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_IGNORE, WTD_UI_NONE,
};
use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};

#[derive(Debug, Clone)]
pub struct ScannedArtifact {
    pub pid: u32,
    pub path: String,
    pub executable_name: String,
    pub sha256_hash: Option<String>,
    pub is_signed: bool,
}

/// enumerates all running processes, extracts paths, hashes them, and checks signatures
pub fn scan_host_artifacts() -> Vec<ScannedArtifact> {
    let mut artifacts = Vec::new();
    let mut pids = [0u32; 2048];
    let mut bytes_returned = 0;

    // grab the process list
    unsafe {
        if EnumProcesses(
            pids.as_mut_ptr(),
            std::mem::size_of_val(&pids) as u32,
            &mut bytes_returned,
        ) == 0
        {
            return artifacts; // failed to enumerate, bail out
        }
    }

    let num_processes = bytes_returned as usize / std::mem::size_of::<u32>();

    for &pid in &pids[..num_processes] {
        if pid == 0 {
            continue;
        }

        if let Some(path) = get_process_path(pid) {
            let executable_name = PathBuf::from(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let sha256_hash = compute_sha256(&path);
            let is_signed = verify_digital_signature(&path);

            artifacts.push(ScannedArtifact {
                pid,
                path,
                executable_name,
                sha256_hash,
                is_signed,
            });
        }
    }

    artifacts
}

/// opens a process and queries its full image path
fn get_process_path(pid: u32) -> Option<String> {
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }

        let mut buffer = [0u16; MAX_PATH as usize];
        let mut size = buffer.len() as u32;

        let success = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(handle);

        if success != 0 {
            let os_string = OsString::from_wide(&buffer[..size as usize]);
            return os_string.into_string().ok();
        }
        None
    }
}

/// computes the sha256 hash of a file efficiently using a buffer
fn compute_sha256(path: &str) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let count = file.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }

    Some(format!("{:x}", hasher.finalize()))
}

/// uses wintrust api to check if the executable has a valid digital signature
fn verify_digital_signature(path: &str) -> bool {
    // convert path to wide string for win32 api
    let mut wide_path: Vec<u16> = path.encode_utf16().collect();
    wide_path.push(0);

    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide_path.as_ptr(),
        hFile: std::ptr::null_mut(),
        pgKnownSubject: std::ptr::null_mut(),
    };

    let mut trust_data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: std::ptr::null_mut(),
        pSIPClientData: std::ptr::null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: windows_sys::Win32::Security::Cryptography::WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_IGNORE,
        hWVTStateData: std::ptr::null_mut(),
        pwszURLReference: std::ptr::null_mut(),
        dwProvFlags: 0,
        dwUIContext: 0,
        pSignatureSettings: std::ptr::null_mut(),
    };

    let mut action_id = WINTRUST_ACTION_GENERIC_VERIFY_V2;

    unsafe {
        let status = WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action_id,
            &mut trust_data as *mut _ as *mut std::ffi::c_void,
        );
        
        // ERROR_SUCCESS (0) means the signature is valid and trusted
        status == ERROR_SUCCESS as i32
    }
}