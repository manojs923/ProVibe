//og lines 768
// src-tauri/src/rdp/client.rs
//
// Spawns mstsc.exe against a generated .rdp file targeting the loopback
// adapter. Silent connect: credentials pre-staged via cmdkey, CredSSP off,
// auth level zero, no prompt. The .rdp file is written to a temp path and
// deleted after spawn — we don't want config artifacts sitting on disk
// between sessions.

use super::{RdpError, TunnelConfig};
use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    WM_SYSCOMMAND, SC_CLOSE,
};

/// How long wait_for_desktop() polls before giving up.
const DESKTOP_TIMEOUT: Duration = Duration::from_secs(45);
/// Poll interval while waiting for the logon desktop to paint.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// WM_GETTEXT buffer size for window title reads.
const TITLE_BUF_LEN: usize = 512;

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
    /// Locate the top-level mstsc window owned by `self.pid` and whose
    /// title starts with the loopback address (which is how the live
    /// session window is distinguished from the connection dialog).
    pub fn get_window_handle(&self) -> Result<HWND, windows::core::Error> {
        let prefix = format!("{} - ", self.loopback_addr);
        SEARCH_PREFIX.with(|p| *p.borrow_mut() = prefix);
        SEARCH_PID.with(|p| p.set(Some(self.pid)));
        SEARCH_RESULT.with(|r| r.set(None));

        unsafe {
            // EnumWindows returns Err if the callback returned FALSE to
            // stop enumeration — that's actually our success signal.
            let _ = EnumWindows(Some(enum_cb_pid), LPARAM(0));
        }

        SEARCH_PID.with(|p| p.set(None));

        match SEARCH_RESULT.with(|r| r.get()) {
            Some(h) => Ok(HWND(h as *mut core::ffi::c_void)),
            None => Err(windows::core::Error::from_win32()),
        }
    }

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
            window_title: format!("{} - Remote Desktop Connection", self.tunnel.loopback_addr),
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

    /// Resolve mstsc.exe path. System32 is not always "System32" under
    /// WoW64, so we ask the environment first.
    fn resolve_mstsc(&self) -> Result<String, RdpError> {
        if !self.config.mstsc_path.is_empty() {
            return Ok(self.config.mstsc_path.clone());
        }
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let path = format!(r"{}\System32\mstsc.exe", sysroot);
        if PathBuf::from(&path).exists() {
            Ok(path)
        } else {
            Err(RdpError::ClientSpawn(format!(
                "mstsc.exe not found at {}",
                path
            )))
        }
    }

    /// Stage credentials so mstsc never paints a credential prompt.
    /// cmdkey /generic maps to the "TERMSRV/<host>" entry mstsc consults.
    ///
    /// We delete any stale entry first — a mismatched stored credential
    /// causes the exact prompt we're trying to avoid.
    fn stage_credentials(&self) -> Result<(), RdpError> {
        let host = &self.tunnel.loopback_addr;
        let general = format!("TERMSRV/{}", host);

        // Best-effort cleanup; a missing entry is not an error.
        let _ = Command::new("cmdkey")
            .args(["/delete", &general])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        // /user accepts "user" or "domain\user" forms.
        let user = match &self.tunnel.domain {
            Some(d) => format!(r"{}\{}", d, self.tunnel.username),
            None => self.tunnel.username.clone(),
        };

        let status = Command::new("cmdkey")
            .args([
                format!("/generic:{}", general),
                format!("/user:{}", user),
                format!("/pass:{}", self.tunnel.password),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| RdpError::ClientSpawn(format!("cmdkey spawn failed: {}", e)))?;

        if !status.success() {
            return Err(RdpError::ClientSpawn(
                "cmdkey failed to store credentials".into(),
            ));
        }
        Ok(())
    }

    /// Generate the .rdp payload. Every line matters — the five keys in the
    /// module docstring are what make the connect fully silent. Notably:
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
        if let Some(d) = &c.domain {
            s.push_str(&format!("domain:s:{}\r\n", d));
        } else {
            s.push_str("domain:s:\r\n");
        }

        // Suppress reconnect/certificate dialogs.
        s.push_str("autoreconnection enabled:i:1\r\n");
        s.push_str("disable themes:i:0\r\n");
        s.push_str("bitmapcachepersistable:i:1\r\n");
        s.push_str("allow font smoothing:i:1\r\n");
        s.push_str("allow desktop composition:i:1\r\n");

        s
    }

    /// Write the .rdp file to a random temp path and return it.
    fn write_rdp_file(&self) -> Result<PathBuf, RdpError> {
        let contents = self.render_rdp_contents();
        let pid_hint = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let fname = format!("s_{}_{}.rdp", pid_hint, nanos);

        let path = std::env::temp_dir().join(fname);
        let mut f = fs::File::create(&path)
            .map_err(|e| RdpError::ClientSpawn(format!("rdp file create failed: {}", e)))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| RdpError::ClientSpawn(format!("rdp file write failed: {}", e)))?;
        Ok(path)
    }

    /// Stage credentials, write .rdp, launch mstsc /f against it.
    pub fn spawn(&mut self) -> Result<(), RdpError> {
        if self.child.is_some() {
            return Err(RdpError::ClientSpawn(
                "client already spawned — teardown() first".into(),
            ));
        }

        let mstsc = self.resolve_mstsc()?;
        self.stage_credentials()?;
        let rdp_path = self.write_rdp_file()?;

        let child = Command::new(&mstsc)
            .arg("/f")
            .arg(&rdp_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| RdpError::ClientSpawn(format!("mstsc spawn failed: {}", e)))?;

        self.child = Some(child);
        self.rdp_file = Some(rdp_path);
        Ok(())
    }

    /// Poll until the live-session window exists, or timeout.
    pub fn wait_for_desktop(&mut self) -> Result<(), std::time::Duration> {
        let start = Instant::now();
        let deadline = start + self.config.desktop_timeout;

        while Instant::now() < deadline {
            if !self.is_alive() {
                return Err(start.elapsed().max(POLL_INTERVAL));
            }

            if let Some(h) = find_live_window(self.tunnel.loopback_addr.as_str()) {
                if window_pid(h) == self.child.as_ref().map(|c| c.id()) {
                    self.hwnd = Some(h);
                    self.connected_once = true;
                    self.cleanup_rdp_file();
                    return Ok(());
                }
            }
                        thread::sleep(POLL_INTERVAL);
        }

        Err(self.config.desktop_timeout)
    }

    /// Terminate the mstsc child, orphan sweep any surviving windows.
    pub fn kill(&mut self) -> Result<(), RdpError> {
        if let Some(h) = self.hwnd.take() {
            unsafe {
                // WM_SYSCOMMAND / SC_CLOSE = user-initiated close path, so
                // mstsc logs off the guest session cleanly before exit.
                let _ = PostMessageW(
                    Some(h),
                    0x0112,          // WM_SYSCOMMAND
                    WPARAM(0xF060),  // SC_CLOSE
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

    fn cleanup_rdp_file(&self) {
        if let Some(p) = &self.rdp_file {
            let _ = fs::remove_file(p);
        }
    }
}

impl Drop for RdpClient {
    fn drop(&mut self) {
        // mod.rs's RDPTunnel::Drop already calls teardown, which calls
        // kill() — this is the last-resort layer for direct RdpClient use.
        if self.child.is_some() {
            let _ = self.kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Window discovery — uses the `windows` crate throughout (no raw FFI blocks).
// ---------------------------------------------------------------------------

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM, BOOL};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
};

/// Length of the window-title scratch buffer used during EnumWindows.
const TITLE_BUF_LEN: usize = 512;

thread_local! {
    static SEARCH_PREFIX: std::cell::RefCell<String> =
        std::cell::RefCell::new(String::new());
    static SEARCH_RESULT: std::cell::Cell<Option<HWND>> =
        const { std::cell::Cell::new(None) };
}

/// Callback invoked by `EnumWindows`. Captures the first visible top-level
/// window whose title starts with the loopback-prefix stored in
/// `SEARCH_PREFIX` *and* contains the live-session marker.
unsafe extern "system" fn enum_cb(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1); // keep enumerating
        }

        let mut buf = [0u16; TITLE_BUF_LEN];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len <= 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&buf[..len as usize]);

        let matched = SEARCH_PREFIX.with(|p| {
            let prefix = p.borrow();
            !prefix.is_empty()
                && title.starts_with(prefix.as_str())
                && title.contains("Remote Desktop Connection")
        });

        if matched {
            SEARCH_RESULT.with(|r| r.set(Some(hwnd)));
            return BOOL(0); // stop enumeration
        }
    }
    BOOL(1)
}

/// Scan top-level windows for a live mstsc session targeting `host`.
pub fn find_live_window(host: &str) -> Option<HWND> {
    SEARCH_PREFIX.with(|p| *p.borrow_mut() = format!("{} - ", host));
    SEARCH_RESULT.with(|r| r.set(None));
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(0));
    }
    SEARCH_RESULT.with(|r| r.get())
}

/// Return the PID that owns `hwnd`, or `None` if the lookup fails.
fn window_pid(hwnd: HWND) -> Option<u32> {
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0 { None } else { Some(pid) }
}

// ---------------------------------------------------------------------------
// RDPClientHandle — thin, serializable snapshot of a live client used by
// InputForwarder and other subsystems that only need PID + window metadata.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RDPClientHandle {
    pub pid: u32,
    pub window_title: String,
    pub loopback_addr: String,
    pub guest_width: u32,
    pub guest_height: u32,
}

impl RDPClientHandle {
    /// Build a handle from a live `RdpClient` once `wait_for_desktop`
    /// has returned successfully.
    pub fn from_client(client: &RdpClient) -> Option<Self> {
        let child = client.child.as_ref()?;
        Some(Self {
            pid: child.id(),
            window_title: format!("{} - Remote Desktop Connection", client.tunnel.loopback_addr),
            loopback_addr: client.tunnel.loopback_addr.clone(),
            guest_width: client.tunnel.desktop_width,
            guest_height: client.tunnel.desktop_height,
        })
    }

    /// Resolve the current live window belonging to our PID. Returns an
    /// error if no matching visible window exists (minimized to tray,
    /// disconnected, process died).
    pub fn get_window_handle(&self) -> Result<HWND, windows::core::Error> {
        let prefix = format!("{} - ", self.loopback_addr);
        SEARCH_PREFIX.with(|p| *p.borrow_mut() = prefix);
        SEARCH_RESULT.with(|r| r.set(None));

        unsafe {
            let _ = EnumWindows(Some(enum_cb_pid), LPARAM(self.pid as isize));
        }

        SEARCH_RESULT
            .with(|r| r.get())
            .ok_or_else(|| windows::core::Error::from_win32())
    }

    /// Guest-side desktop resolution the RDP session negotiated.
    pub fn get_guest_resolution(&self) -> (u32, u32) {
        (self.guest_width, self.guest_height)
    }
}

/// PID-scoped variant of `enum_cb` — used by `RDPClientHandle::get_window_handle`.
unsafe extern "system" fn enum_cb_pid(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }

        let target_pid = lparam.0 as u32;
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != target_pid {
            return BOOL(1);
        }

        let mut buf = [0u16; TITLE_BUF_LEN];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len <= 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&buf[..len as usize]);

        let matched = SEARCH_PREFIX.with(|p| {
            let prefix = p.borrow();
            !prefix.is_empty()
                && title.starts_with(prefix.as_str())
                && title.contains("Remote Desktop Connection")
        });

        if matched {
            SEARCH_RESULT.with(|r| r.set(Some(hwnd)));
            return BOOL(0);
        }
    }
    BOOL(1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base_tunnel() -> TunnelConfig {
        TunnelConfig::default()
    }

    #[test]
    fn rdp_contents_contains_silent_connect_keys() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("full address:s:127.0.0.2"));
        assert!(s.contains("authentication level:i:0"));
        assert!(s.contains("enablecredsspsupport:i:0"));
        assert!(s.contains("prompt for credentials:i:0"));
        assert!(s.contains("negotiate security layer:i:0"));
    }

    #[test]
    fn rdp_contents_uses_configured_dimensions() {
        let mut tunnel = base_tunnel();
        tunnel.desktop_width = 2560;
        tunnel.desktop_height = 1440;
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("desktopwidth:i:2560"));
        assert!(s.contains("desktopheight:i:1440"));
    }

    #[test]
    fn rdp_contents_defaults_to_1080p() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("desktopwidth:i:1920"));
        assert!(s.contains("desktopheight:i:1080"));
    }

    #[test]
    fn rdp_contents_contains_username_when_set() {
        let mut tunnel = base_tunnel();
        tunnel.username = "testuser".to_string();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("username:s:testuser"));
    }

    #[test]
    fn rdp_contents_contains_domain_when_set() {
        let mut tunnel = base_tunnel();
        tunnel.username = "admin".to_string();
        tunnel.domain = Some("LOCALHOST".to_string());
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("domain:s:LOCALHOST"));
        assert!(s.contains("username:s:admin"));
    }

    #[test]
    fn rdp_contents_omits_domain_when_absent() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(!s.contains("domain:s:"));
    }

    #[test]
    fn rdp_contents_audio_follows_config_flag() {
        let mut tunnel = base_tunnel();
        tunnel.audio_redirection = true;
        let client = RdpClient::new(&tunnel);
        assert!(client.render_rdp_contents().contains("audiocapturemode:i:1"));

        tunnel.audio_redirection = false;
        let client = RdpClient::new(&tunnel);
        assert!(client.render_rdp_contents().contains("audiocapturemode:i:0"));
    }

    #[test]
    fn rdp_contents_peripheral_redirects_locked_down() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("redirectprinters:i:0"));
        assert!(s.contains("redirectclipboard:i:0"));
        assert!(s.contains("redirectcomports:i:0"));
        assert!(s.contains("redirectsmartcards:i:0"));
        assert!(s.contains("redirectposdevices:i:0"));
        assert!(s.contains("redirectdrives:i:0"));
        assert!(s.contains("drivestoredirect:s:"));
    }

    #[test]
    fn rdp_contents_autoreconnect_enabled() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        assert!(client
            .render_rdp_contents()
            .contains("autoreconnection enabled:i:1"));
    }

    #[test]
    fn rdp_contents_endings_are_crlf() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();
        assert!(s.contains("\r\n"));
    }

    #[test]
    fn rdp_contents_is_ascii_safe() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        assert!(client.render_rdp_contents().is_ascii());
    }

    #[test]
    fn render_is_deterministic_for_identical_config() {
        let tunnel = base_tunnel();
        let a = RdpClient::new(&tunnel).render_rdp_contents();
        let b = RdpClient::new(&tunnel).render_rdp_contents();
        assert_eq!(a, b);
    }

    #[test]
    fn fresh_client_never_connected() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        assert!(!client.ever_connected());
        assert!(client.live_hwnd().is_none());
    }

    #[test]
    fn write_rdp_file_round_trips() {
        let tunnel = base_tunnel();
        let client = RdpClient::new(&tunnel);
        let path = client.write_rdp_file().expect("write failed");
        assert!(path.exists());

        let contents = fs::read_to_string(&path).expect("read failed");
        assert!(contents.contains("full address:s:127.0.0.2"));
        assert!(contents.contains("desktopwidth:i:1920"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn default_rdp_client_config_sane() {
        let cfg = RdpClientConfig::default();
        assert_eq!(cfg.desktop_timeout, DESKTOP_TIMEOUT);
        assert_eq!(cfg.live_title_marker, "Remote Desktop Connection");
        assert!(cfg.mstsc_path.is_empty());
    }

    #[test]
    fn handle_resolution_matches_tunnel_config() {
        let mut tunnel = base_tunnel();
        tunnel.desktop_width = 3840;
        tunnel.desktop_height = 2160;

        // Fabricate a handle without spawning (test env has no mstsc).
        let handle = RDPClientHandle {
            pid: 0,
            window_title: "test".to_string(),
            loopback_addr: tunnel.loopback_addr.clone(),
            guest_width: tunnel.desktop_width,
            guest_height: tunnel.desktop_height,
        };

        assert_eq!(handle.get_guest_resolution(), (3840, 2160));
    }
}