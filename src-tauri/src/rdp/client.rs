// src-tauri/src/rdp/client.rs
//
// Spawns mstsc.exe against a generated .rdp file targeting the loopback
// adapter. Silent connect: credentials pre-staged via cmdkey, CredSSP off,
// auth level zero, no prompt. The .rdp file is written to a temp path and
// deleted after spawn — we don't want config artifacts sitting on disk
// between sessions.
//
// Every failure here is a `ClientError`; `rdp::RdpError` wraps it with
// `#[from]`, so callers use `?` and keep their own taxonomy.

use super::TunnelConfig;
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use windows::core::BOOL;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    WM_SYSCOMMAND,
};

/// How long wait_for_desktop() polls before giving up.
const DESKTOP_TIMEOUT: Duration = Duration::from_secs(45);
/// Poll interval while waiting for the logon desktop to paint.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// WM_GETTEXT buffer size for window title reads.
const TITLE_BUF_LEN: usize = 512;
/// SC_CLOSE wParam for WM_SYSCOMMAND. Kept as a literal so we don't depend
/// on whether a given windows-rs release types it as u32 or WPARAM.
const SC_CLOSE_WPARAM: usize = 0xF060;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A client-spawn or client-lifecycle failure.
///
/// Split from `rdp::RdpError` so the spawn path can be reasoned about on its
/// own — `ConfigIo` is fixable, `NotRunning` is a retry, `InvalidPath` is not.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The client process could not be launched.
    #[error("failed to launch client: {0}")]
    Spawn(String),

    /// The desktop did not paint inside the configured deadline.
    #[error("desktop did not appear within {0:?}")]
    Timeout(Duration),

    /// The client process is gone.
    #[error("client is not running")]
    NotRunning,

    /// mstsc.exe could not be located on this host.
    #[error("invalid client path: {0}")]
    InvalidPath(String),

    /// Credential staging through cmdkey failed.
    #[error("failed to stage credentials: {0}")]
    StageCredentials(String),

    /// The generated .rdp config could not be written.
    #[error("rdp config io error: {0}")]
    ConfigIo(String),
}

/// Convert a raw HWND value (as stored in our handle structs) back to HWND.
#[inline]
fn hwnd_from_raw(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

#[derive(Debug, Clone)]
pub struct RdpClientConfig {
    /// Full path to mstsc.exe. Resolved from System32 at spawn time unless
    /// overridden (useful for tests / odd WoW64 setups).
    pub mstsc_path: String,
    /// Max time to wait for the desktop to render after launch.
    pub desktop_timeout: Duration,
    /// Window title substring that indicates a *connected* session.
    /// mstsc titles its window "<remote host> - Remote Desktop Connection"
    /// only while the session is live; error dialogs and the initial
    /// connection dialog use other titles.
    pub live_title_marker: String,
}

impl Default for RdpClientConfig {
    fn default() -> Self {
        Self {
            mstsc_path: String::new(), // resolved lazily
            desktop_timeout: DESKTOP_TIMEOUT,
            live_title_marker: "Remote Desktop Connection".to_string(),
        }
    }
}

/// One spawned mstsc.exe instance.
///
/// Lifecycle:
///   new()      -> resolve paths, stage credentials, nothing on screen yet
///   spawn()    -> write .rdp, launch mstsc /f, take ownership of Child
///   wait_for_desktop() -> poll until the live-session window exists
///   kill()     -> terminate child + orphan sweep
pub struct RdpClient {
    config: RdpClientConfig,
    tunnel: TunnelConfig,
    child: Option<Child>,
    rdp_file: Option<PathBuf>,
    /// mstsc's connection dialog and live window are separate HWNDs; we
    /// cache the live one once found so stream.rs can attach by handle.
    hwnd: Option<isize>,
    connected_once: bool,
}

/// Lightweight external-facing handle used by `input.rs` and other
/// subsystems that need to locate the mstsc window without owning the
/// `RdpClient` itself. Produced via `RdpClient::handle()`.
#[derive(Debug, Clone)]
pub struct RDPClientHandle {
    pub pid: u32,
    pub window_title: String,
    pub loopback_addr: String,
    pub guest_width: u32,
    pub guest_height: u32,
}

impl RDPClientHandle {
    /// Process id of the spawned client.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Title the live-session window is expected to carry.
    pub fn window_title(&self) -> String {
        self.window_title.clone()
    }

    /// Locate the top-level mstsc window owned by `self.pid` and whose
    /// title starts with the loopback address (which is how the live
    /// session window is distinguished from the connection dialog).
    pub fn get_window_handle(&self) -> Result<HWND, windows::core::Error> {
        let prefix = format!("{} - ", self.loopback_addr);
        match find_window(&prefix, Some(self.pid)) {
            Some(raw) => Ok(hwnd_from_raw(raw)),
            None => Err(windows::core::Error::from_win32()),
        }
    }

    /// Guest-side desktop resolution the RDP session negotiated.
    pub fn get_guest_resolution(&self) -> (u32, u32) {
        (self.guest_width, self.guest_height)
    }
}

impl RdpClient {
    pub fn process_id(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    pub fn new(tunnel: &TunnelConfig) -> Self {
        Self {
            config: RdpClientConfig::default(),
            tunnel: tunnel.clone(),
            child: None,
            rdp_file: None,
            hwnd: None,
            connected_once: false,
        }
    }

    pub fn with_config(mut self, config: RdpClientConfig) -> Self {
        self.config = config;
        self
    }

    /// Produce the external-facing handle other subsystems consume.
    pub fn handle(&self) -> Option<RDPClientHandle> {
        let pid = self.process_id()?;
        Some(RDPClientHandle {
            pid,
            window_title: format!(
                "{} - {}",
                self.tunnel.loopback_addr, self.config.live_title_marker
            ),
            loopback_addr: self.tunnel.loopback_addr.clone(),
            guest_width: self.tunnel.desktop_width,
            guest_height: self.tunnel.desktop_height,
        })
    }

    /// Did we ever reach a live desktop in this client's lifetime?
    pub fn ever_connected(&self) -> bool {
        self.connected_once
    }

    /// Is the mstsc child process still breathing?
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => c.try_wait().map(|w| w.is_none()).unwrap_or(false),
            None => false,
        }
    }

    /// The live-session HWND, once wait_for_desktop() has located it.
    pub fn live_hwnd(&self) -> Option<isize> {
        self.hwnd
    }

    /// Credential / .rdp identity for the tunnel, in "user" or "domain\user"
    /// form. An empty domain means a local account.
    fn qualified_user(&self) -> String {
        if self.tunnel.domain.is_empty() {
            self.tunnel.username.clone()
        } else {
            format!(r"{}\{}", self.tunnel.domain, self.tunnel.username)
        }
    }

    /// Resolve mstsc.exe path. System32 is not always "System32" under
    /// WoW64, so we probe the native view first and fall back.
    fn resolve_mstsc(&self) -> Result<String, ClientError> {
        if !self.config.mstsc_path.is_empty() {
            return Ok(self.config.mstsc_path.clone());
        }
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        for dir in ["System32", "Sysnative"] {
            let path = format!(r"{}\{}\mstsc.exe", sysroot, dir);
            if PathBuf::from(&path).exists() {
                return Ok(path);
            }
        }
        Err(ClientError::InvalidPath(format!(
            "mstsc.exe not found under {}\\System32",
            sysroot
        )))
    }

    /// Stage credentials so mstsc never paints a credential prompt.
    /// cmdkey /generic maps to the "TERMSRV/<host>" entry mstsc consults.
    ///
    /// We delete any stale entry first — a mismatched stored credential
    /// causes the exact prompt we're trying to avoid.
    fn stage_credentials(&self) -> Result<(), ClientError> {
        let host = &self.tunnel.loopback_addr;
        let general = format!("TERMSRV/{}", host);

        // Best-effort cleanup; a missing entry is not an error.
        let _ = Command::new("cmdkey")
            .args(["/delete", &general])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let status = Command::new("cmdkey")
            .args([
                format!("/generic:{}", general),
                format!("/user:{}", self.qualified_user()),
                format!("/pass:{}", self.tunnel.password),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| ClientError::StageCredentials(format!("cmdkey spawn failed: {}", e)))?;

        if !status.success() {
            return Err(ClientError::StageCredentials(
                "cmdkey failed to store credentials".into(),
            ));
        }
        Ok(())
    }

    /// Generate the .rdp payload. Every line matters — the silent-connect
    /// keys are what make the connect fully prompt-free. Notably:
    ///   - authentication level:i:0 -> no server cert warnings
    ///   - enablecredsspsupport:i:0 -> don't require network-level auth
    ///   - prompt for credentials:i:0 -> trust the stored cmdkey entry
    fn render_rdp_contents(&self) -> String {
        let mut s = String::with_capacity(2048);
        let c = &self.tunnel;
        let w = c.desktop_width;
        let h = c.desktop_height;

        // Core addressing + silent auth.
        s.push_str(&format!("full address:s:{}\r\n", c.loopback_addr));
        s.push_str("authentication level:i:0\r\n");
        s.push_str("enablecredsspsupport:i:0\r\n");
        s.push_str("prompt for credentials:i:0\r\n");
        s.push_str("prompt for credentials on client:i:0\r\n");
        s.push_str("promptcredentialonce:i:0\r\n");
        s.push_str("negotiate security layer:i:0\r\n");

        // Render geometry.
        s.push_str(&format!("desktopwidth:i:{}\r\n", w));
        s.push_str(&format!("desktopheight:i:{}\r\n", h));
        s.push_str("smart sizing:i:0\r\n");
        s.push_str("screen mode id:i:1\r\n"); // 1 = windowed, 2 = full screen
        s.push_str("session bpp:i:32\r\n");
        s.push_str("compression:i:1\r\n");

        // Local resource mapping.
        s.push_str(&format!(
            "audiocapturemode:i:{}\r\n",
            if c.audio_redirection { 1 } else { 0 }
        ));
        s.push_str("audiomode:i:0\r\n"); // play audio locally
        s.push_str("redirectprinters:i:0\r\n");
        s.push_str("redirectclipboard:i:0\r\n");
        s.push_str("redirectcomports:i:0\r\n");
        s.push_str("redirectsmartcards:i:0\r\n");
        s.push_str("redirectposdevices:i:0\r\n");
        s.push_str("drivestoredirect:s:\r\n");
        s.push_str("redirectdrives:i:0\r\n");

        // Credential hint.
        s.push_str(&format!("username:s:{}\r\n", c.username));
        s.push_str(&format!("domain:s:{}\r\n", c.domain));

        // Suppress reconnect/certificate dialogs.
        s.push_str("autoreconnection enabled:i:1\r\n");
        s.push_str("disable themes:i:0\r\n");
        s.push_str("bitmapcachepersistable:i:1\r\n");
        s.push_str("allow font smoothing:i:1\r\n");
        s.push_str("allow desktop composition:i:1\r\n");

        s
    }

    /// Write the .rdp file to a random temp path and return it.
    fn write_rdp_file(&self) -> Result<PathBuf, ClientError> {
        let contents = self.render_rdp_contents();
        let pid_hint = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let fname = format!("s_{}_{}.rdp", pid_hint, nanos);

        let path = std::env::temp_dir().join(fname);
        let mut f = fs::File::create(&path)
            .map_err(|e| ClientError::ConfigIo(format!("rdp file create failed: {}", e)))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| ClientError::ConfigIo(format!("rdp file write failed: {}", e)))?;
        Ok(path)
    }

    /// Stage credentials, write .rdp, launch mstsc /f against it.
    pub fn spawn(&mut self) -> Result<(), ClientError> {
        if self.child.is_some() {
            return Err(ClientError::Spawn(
                "client already spawned — teardown() first".into(),
            ));
        }

        let mstsc = self.resolve_mstsc()?;
        self.stage_credentials()?;
        let rdp_path = self.write_rdp_file()?;

        let child = match Command::new(&mstsc)
            .arg("/f")
            .arg(&rdp_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                // Don't leave the temp .rdp behind if the launch failed.
                let _ = fs::remove_file(&rdp_path);
                return Err(ClientError::Spawn(format!("mstsc spawn failed: {}", e)));
            }
        };

        self.child = Some(child);
        self.rdp_file = Some(rdp_path);
        Ok(())
    }

    /// Poll until the live-session window exists, or timeout.
    pub fn wait_for_desktop(&mut self) -> Result<(), ClientError> {
        let deadline = Instant::now() + self.config.desktop_timeout;

        let pid = self.process_id().ok_or(ClientError::NotRunning)?;
        let prefix = format!("{} - ", self.tunnel.loopback_addr);

        while Instant::now() < deadline {
            if !self.is_alive() {
                return Err(ClientError::NotRunning);
            }

            if let Some(h) = find_window(&prefix, Some(pid)) {
                if window_pid(h) == Some(pid) {
                    self.hwnd = Some(h);
                    self.connected_once = true;
                    self.cleanup_rdp_file();
                    return Ok(());
                }
            }

            thread::sleep(POLL_INTERVAL);
        }

        Err(ClientError::Timeout(self.config.desktop_timeout))
    }

    /// Terminate the mstsc child, orphan sweep any surviving windows.
    pub fn kill(&mut self) -> Result<(), ClientError> {
        if let Some(h) = self.hwnd.take() {
            unsafe {
                // WM_SYSCOMMAND / SC_CLOSE = user-initiated close path, so
                // mstsc logs off the guest session cleanly before exit.
                let _ = PostMessageW(
                    Some(hwnd_from_raw(h)),
                    WM_SYSCOMMAND,
                    WPARAM(SC_CLOSE_WPARAM),
                    LPARAM(0),
                );
            }
        }

        if let Some(mut c) = self.child.take() {
            // Give WM_CLOSE a brief grace window before we hard-kill.
            let grace_deadline = Instant::now() + Duration::from_millis(1500);
            loop {
                match c.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() >= grace_deadline => {
                        let _ = c.kill();
                        let _ = c.wait();
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                    Err(_) => {
                        let _ = c.kill();
                        break;
                    }
                }
            }
        }

        self.cleanup_rdp_file();
        self.connected_once = false;
        Ok(())
    }

    /// Graceful teardown. Async to match the tunnel's shutdown path; the
    /// body is deliberately synchronous so teardown still works from a
    /// runtime-free context via `kill()`.
    pub async fn disconnect(&mut self) -> Result<(), ClientError> {
        self.kill()
    }

    fn cleanup_rdp_file(&mut self) {
        if let Some(p) = self.rdp_file.take() {
            let _ = fs::remove_file(p);
        }
    }
}

impl Drop for RdpClient {
    fn drop(&mut self) {
        // mod.rs's Tunnel::Drop already calls shutdown, which calls
        // disconnect — this is the last-resort layer for direct RdpClient use.
        if self.child.is_some() {
            let _ = self.kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Window search plumbing
//
// EnumWindows needs a plain fn pointer, so the search parameters travel
// through thread-locals. The callback returns FALSE to stop enumeration
// early; EnumWindows then reports Err, which we deliberately ignore because
// "stopped early" is our success path.
// ---------------------------------------------------------------------------

thread_local! {
    static SEARCH_PREFIX: RefCell<String> = const { RefCell::new(String::new()) };
    static SEARCH_PID: Cell<Option<u32>> = const { Cell::new(None) };
    static SEARCH_RESULT: Cell<Option<isize>> = const { Cell::new(None) };
}

/// Read a window's title, honoring the length GetWindowTextW reports so we
/// don't scan the whole zero-filled buffer.
fn get_window_title(hwnd: HWND) -> String {
    let mut buffer = [0u16; TITLE_BUF_LEN];

    let len = unsafe { GetWindowTextW(hwnd, &mut buffer[..]) };
    if len <= 0 {
        return String::new();
    }

    let len = (len as usize).min(buffer.len());
    OsString::from_wide(&buffer[..len])
        .to_string_lossy()
        .into_owned()
}

unsafe extern "system" fn enum_cb_pid(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let _ = lparam;

    let Some(pid) = SEARCH_PID.with(|p| p.get()) else {
        return TRUE;
    };

    if window_pid_raw(hwnd) != Some(pid) || !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return TRUE;
    }

    let title = get_window_title(hwnd);
    if title.is_empty() {
        return TRUE;
    }

    let matches = SEARCH_PREFIX.with(|p| {
        let prefix = p.borrow();
        prefix.is_empty() || title.starts_with(prefix.as_str())
    });

    if matches {
        SEARCH_RESULT.with(|r| r.set(Some(hwnd.0 as isize)));
        return FALSE; // stop enumeration — we have our window
    }

    TRUE
}

/// Find the first visible top-level window whose title starts with `prefix`
/// and which belongs to `pid`.
fn find_window(prefix: &str, pid: Option<u32>) -> Option<isize> {
    SEARCH_PREFIX.with(|p| *p.borrow_mut() = prefix.to_string());
    SEARCH_PID.with(|p| p.set(pid));
    SEARCH_RESULT.with(|r| r.set(None));

    unsafe {
        // Err here means either a genuine failure or that the callback
        // returned FALSE. Both are handled by inspecting SEARCH_RESULT.
        let _ = EnumWindows(Some(enum_cb_pid), LPARAM(0));
    }

    let found = SEARCH_RESULT.with(|r| r.get());

    SEARCH_PID.with(|p| p.set(None));
    SEARCH_PREFIX.with(|p| p.borrow_mut().clear());

    found
}

/// Process id owning `hwnd`, or None if the window is gone.
fn window_pid(hwnd: isize) -> Option<u32> {
    window_pid_raw(hwnd_from_raw(hwnd))
}

fn window_pid_raw(hwnd: HWND) -> Option<u32> {
    let mut pid: u32 = 0;
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if tid != 0 && pid != 0 {
        Some(pid)
    } else {
        None
    }
}