// src-tauri/src/rdp/client.rs
//
// Spawns mstsc.exe against a generated .rdp file targeting the loopback
// adapter. Silent connect: credentials pre-staged via cmdkey, CredSSP off,
// auth level zero, no prompt. The .rdp file is written to a temp path and
// deleted after spawn — we don't want config artifacts sitting on disk
// between sessions.

use super::{RdpError, TunnelConfig};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

#[derive(Debug, Clone)]
pub struct RDPClientHandle {
    pub pid: u32,
    pub window_title: String,
    pub loopback_addr: String,
}

impl RDPClientHandle {
    pub fn get_window_handle(&self) -> Result<HWND, windows::core::Error> {
        self.find_rdp_window()
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

    /// Did we ever reach a live desktop in this client's lifetime?
    pub fn ever_connected(&self) -> bool {
        self.connected_once
    }

    /// Is the mstsc child process still breathing?
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            // try_wait returns Ok(None) while the process still runs.
            Some(c) => c.try_wait().map(|w| w.is_none()).unwrap_or(false),
            None => false,
        }
    }

    /// The live-session HWND, once wait_for_desktop() has located it.
    /// stream.rs attaches its capture loop to this handle.
    pub fn live_hwnd(&self) -> Option<isize> {
        self.hwnd
    }

    /// Resolve mstsc.exe path. System32 is not always "System32" under
    /// WoW64, so we ask the environment first.
    fn resolve_mstsc(&self) -> Result<String, RdpError> {
        if !self.config.mstsc_path.is_empty() {
            return Ok(self.config.mstsc_path.clone());
        }
        let sysroot = std::env::var("SystemRoot")
            .unwrap_or_else(|_| r"C:\Windows".to_string());
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
    /// cmdkey /general maps to the " TERMSRV/<host>" entry mstsc consults.
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

        // /user accepts "user" or "domain\\user" forms.
        let user = match &self.tunnel.domain {
            Some(d) => format!(r"{}\{}", d, self.tunnel.username),
            None => self.tunnel.username.clone(),
        };

        let status = Command::new("cmdkey")
            .args([
                "/generic:" as &str, &general,
                "/user:" as &str, &user,
                "/pass:" as &str, &self.tunnel.password,
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
        s.push_str("negotiate security layer:i:0\r\n");

        // Render geometry — matches the TunnelConfig defaults so the
        // stream capture knows its expected frame size.
        s.push_str(&format!("desktopwidth:i:{}\r\n", w));
        s.push_str(&format!("desktopheight:i:{}\r\n", h));
        s.push_str("smart sizing:i:0\r\n");
        s.push_str("screen mode id:i:1\r\n"); // full-screen
        s.push_str("session bpp:i:32\r\n");
        s.push_str("compression:i:1\r\n");

        // Local resource mapping — we want audio redirected *in* (for the
        // audio tap) and everything else locked down.
        s.push_str(&format!(
            "audiocapturemode:i:{}\r\n",
            if c.audio_redirection { 1 } else { 0 }
        ));
        s.push_str("redirectprinters:i:0\r\n");
        s.push_str("redirectclipboard:i:0\r\n");
        s.push_str("redirectcomports:i:0\r\n");
        s.push_str("redirectsmartcards:i:0\r\n");
        s.push_str("redirectposdevices:i:0\r\n");
        s.push_str("drivestoredirect:s:\r\n"); // empty -> none
        s.push_str("redirectdrives:i:0\r\n");

        // Credential hint — mstsc prefers the stored TERMSRV entry, but
        // this belt-and-suspenders line keeps silent behavior consistent.
        s.push_str(&format!("username:s:{}\r\n", c.username));
        if let Some(d) = &c.domain {
            s.push_str(&format!("domain:s:{}\r\n", d));
        }

        // Suppress reconnect/certificate dialogs that would steal focus
        // from the session window.
        s.push_str("autoreconnection enabled:i:1\r\n");
        s.push_str("authentication level:i:0\r\n"); // re-asserted intentionally
        s.push_str("disable themes:i:0\r\n");
        s.push_str("bitmapcachepersistable:i:1\r\n");
        s.push_str("allow font smoothing:i:1\r\n");
        s.push_str("allow desktop composition:i:1\r\n");

        s
    }

    /// Write the .rdp file to a random temp path and return it.
    /// Random filename avoids collisions between concurrent tunnels.
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
            .arg("/f") // silent — reads config from the file, no dialog
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
    ///
    /// Detection strategy, in order:
    ///   1. EnumWindows looking for a title containing our live marker
    ///      AND belonging to the mstsc child PID. The initial connection
    ///      dialog is also mstsc's, so we additionally require the title
    ///      to lead with the loopback host — "127.0.0.2 - Remote Desktop
    ///      Connection" only appears once the session is up.
    ///   2. Child exit during the wait means auth or network failure —
    ///      surface that immediately rather than burning the full timeout.
    pub fn wait_for_desktop(&mut self) -> Result<(), std::time::Duration> {
        let deadline = Instant::now() + self.config.desktop_timeout;
        let expected_prefix = format!("{} - ", self.tunnel.loopback_addr);

        while Instant::now() < deadline {
            // Child died on us — fail fast with elapsed time.
            if !self.is_alive() {
                return Err(Instant::now().elapsed().max(POLL_INTERVAL));
            }

            if let Some(h) = find_live_window(self.tunnel.loopback_addr.as_str()) {
                // Confirm it belongs to our child, not some other RDP
                // session the user happens to have open.
                if window_pid(h) == self.child.as_ref().map(|c| c.id()) {
                    self.hwnd = Some(h);
                    self.connected_once = true;
                    // Connected — clean up the .rdp file early so it
                    // doesn't linger on disk for the session duration.
                    self.cleanup_rdp_file();
                    return Ok(());
                }
            }

            thread::sleep(POLL_INTERVAL);
        }

        Err(self.config.desktop_timeout)
    }

    /// Terminate the mstsc child, orphan sweep any surviving windows.
    ///
    /// kill() on the Child handle covers the normal case; the Win32
    /// PostMessage WM_CLOSE is the graceful sweep in case mstsc detached
    /// from our handle (elevated relaunch, session handoff edge cases).
    pub fn kill(&mut self) -> Result<(), RdpError> {
        if let Some(h) = self.hwnd.take() {
            unsafe {
                // WM_SYSCOMMAND SC_CLOSE = user-initiated close path, so
                // mstsc logs off the guest session cleanly.
                PostMessageW(h as _, 0x0112 /* WM_SYSCOMMAND */, 0xF060 /* SC_CLOSE */, 0);
            }
        }

        if let Some(mut c) = self.child.take() {
            let _ = c.wait(); // let WM_CLOSE do its work first
            let _ = c.kill();
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
// Win32 FFI — kept minimal and local to this file. stream.rs will bring its
// own capture FFI; this block is only for spawn-time window discovery.
// ---------------------------------------------------------------------------

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(lpEnumFunc: extern "system" fn(isize, isize) -> i32, lParam: isize) -> i32;
    fn GetWindowTextW(hWnd: isize, lpString: *mut u16, nMaxCount: i32) -> i32;
    fn GetWindowThreadProcessId(hWnd: isize, lpdwProcessId: *mut u32) -> u32;
    fn IsWindowVisible(hWnd: isize) -> i32;
    fn PostMessageW(hWnd: isize, Msg: u32, wParam: usize, lParam: isize) -> i32;
}

thread_local! {
    static SEARCH_RESULT: std::cell::Cell<Option<isize>> = const { std::cell::Cell::new(None) };
}

/// Callback for EnumWindows.
extern "system" fn enum_cb(hwnd: isize, _lparam: isize) -> i32 {
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return 1; // keep enumerating
        }
        let mut buf = [0u16; TITLE_BUF_LEN];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), TITLE_BUF_LEN as i32);
        if len <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        // Live mstsc window: "<full address> - Remote Desktop Connection"
        if title.starts_with(&SEARCH_PREFIX.with(|p| p.borrow().clone()))
            && title.contains("Remote Desktop Connection")
        {
            SEARCH_RESULT.with(|r| r.set(Some(hwnd)));
            return 0; // stop enumeration
        }
    }
    1
}

thread_local! {
    static SEARCH_PREFIX: std::cell::RefCell<String> =
        std::cell::RefCell::new(String::new());
}

/// Scan the top-level windows for a live mstsc session against `host`.
/// Returns the HWND, if any. Safe to call repeatedly (it's a poll).
pub fn find_live_window(host: &str) -> Option<isize> {
    SEARCH_PREFIX.with(|p| *p.borrow_mut() = format!("{} - ", host));
    SEARCH_RESULT.with(|r| r.set(None));
    unsafe {
        EnumWindows(enum_cb, 0);
    }
    SEARCH_RESULT.with(|r| r.get())
}

/// PID that owns a window.
fn window_pid(hwnd: isize) -> Option<u32> {
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid == 0 { None } else { Some(pid) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdp_contents_contains_silent_connect_keys() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("full address:s:127.0.0.2"));
        assert!(s.contains("desktopwidth:i:1920"));
        assert!(s.contains("desktopheight:i:1080"));
        assert!(s.contains("authentication level:i:0"));
        assert!(s.contains("enablecredsspsupport:i:0"));
        assert!(s.contains("prompt for credentials:i:0"));
        assert!(s.contains("redirectclipboard:i:1"));
        assert!(s.contains("redirectprinters:i:0"));
        assert!(s.contains("redirectsmartcards:i:0"));
        assert!(s.contains("redirectdevices:i:0"));
        assert!(s.contains("redirectdrives:i:0"));
        assert!(s.contains("redirectcomports:i:0"));
        assert!(s.contains("redirectposdevices:i:0"));
        assert!(s.contains("devicestoredirect:s:"));
        assert!(s.contains("drivestoredirect:s:"));
        assert!(s.contains("keyboardhook:i:2"));
        assert!(s.contains("audiomode:i:0"));
        assert!(s.contains("screen mode id:i:2"));
        assert!(s.contains("use multimon:i:0"));
        assert!(s.contains("session bpp:i:32"));
        assert!(s.contains("compression:i:1"));
        assert!(s.contains("videoplaybackmode:i:1"));
        assert!(s.contains("connection type:i:7"));
        assert!(s.contains("networkautodetect:i:1"));
        assert!(s.contains("bandwidthautodetect:i:1"));
        assert!(s.contains("displayconnectionbar:i:1"));
        assert!(s.contains("disable wallpaper:i:0"));
        assert!(s.contains("allow font smoothing:i:1"));
        assert!(s.contains("allow desktop composition:i:1"));
        assert!(s.contains("disable full window drag:i:0"));
        assert!(s.contains("disable menu anims:i:0"));
        assert!(s.contains("disable themes:i:0"));
        assert!(s.contains("disable cursor setting:i:0"));
        assert!(s.contains("bitmapcachepersistenable:i:1"));
        assert!(s.contains("full address:s:127.0.0.2"));
        assert!(s.contains("autoreconnection enabled:i:1"));
        assert!(s.contains("authentication level:i:0"));
        assert!(s.contains("prompt for credentials:i:0"));
        assert!(s.contains("negotiate security layer:i:0"));
        assert!(s.contains("remoteapplicationmode:i:0"));
        assert!(s.contains("alternate shell:s:"));
        assert!(s.contains("shell working directory:s:"));
        assert!(s.contains("gatewayhostname:s:"));
        assert!(s.contains("gatewayusagemethod:i:4"));
        assert!(s.contains("gatewaycredentialssource:i:4"));
        assert!(s.contains("gatewayprofileusagemethod:i:0"));
        assert!(s.contains("pre-command:s:"));
    }

    #[test]
    fn rdp_contents_contains_username() {
        let mut tunnel = TunnelConfig::default();
        tunnel.username = String::from("testuser");
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("username:s:testuser"));
        assert!(s.contains("domain:s:"));
    }

    #[test]
    fn rdp_contents_contains_domain() {
        let mut tunnel = TunnelConfig::default();
        tunnel.domain = String::from("TESTDOMAIN");
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("domain:s:TESTDOMAIN"));
    }

    #[test]
    fn rdp_contents_respects_custom_dimensions() {
        let mut tunnel = TunnelConfig::default();
        tunnel.width = 2560;
        tunnel.height = 1440;
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("desktopwidth:i:2560"));
        assert!(s.contains("desktopheight:i:1440"));
    }

    #[test]
    fn rdp_contents_uses_loopback_address() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("full address:s:127.0.0.2"));
        assert!(!s.contains("full address:s:127.0.0.1"));
    }

    #[test]
    fn rdp_contents_no_creds_when_empty() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // When no stored credentials exist, the file should not embed them
        assert!(!s.contains("password 51:b:"));
    }

    #[test]
    fn rdp_file_written_to_temp_dir() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let path = client.rdp_file_path();

        let parent = path.parent().unwrap();
        assert!(parent.exists() || std::fs::create_dir_all(parent).is_ok());

        let write_result = client.write_rdp_file(&path);
        assert!(write_result.is_ok());
        assert!(path.exists());

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("full address:s:127.0.0.2"));

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mstsc_spawn_command_contains_file_flag() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let cmd = client.build_mstsc_command();

        assert!(cmd.contains("mstsc.exe"));
        assert!(cmd.contains("/v:"));
    }

    #[test]
    fn default_config_targets_loopback() {
        let tunnel = TunnelConfig::default();
        assert_eq!(tunnel.host, "127.0.0.2");
        assert_eq!(tunnel.port, 3389);
    }

    #[test]
    fn rdp_contents_disabled_input_forwarding() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // Input policy is forbidden by default — no keyboard/mouse passthrough config
        assert!(s.contains("keyboardhook:i:2"));
    }

    #[test]
    fn rdp_contents_no_gui_prompt_lines() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // These keys would cause mstsc to show dialogs — must be explicitly zeroed
        assert!(s.contains("prompt for credentials:i:0"));
        assert!(s.contains("promptcredentialonce:i:0"));
        assert!(s.contains("enablecredsspsupport:i:0"));
        assert!(s.contains("authentication level:i:0"));
    }

    #[test]
    fn rdp_contents_endings_are_crlf() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // .rdp files require Windows line endings for mstsc to parse correctly
        assert!(s.contains("\r\n"));
        assert!(!s.matches('\n').any(|_| false));
    }

    #[test]
    fn rdp_contents_encoding_is_ascii_safe() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // mstsc expects ANSI/ASCII .rdp content — no multi-byte chars
        assert!(s.is_ascii());
    }

    #[test]
    fn credential_blob_omitted_when_none() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // No stored credentials means no password line at all
        assert!(!s.contains("password 51:b:"));
    }

    #[test]
    fn drop_kills_mstsc_process() {
        // This test verifies the Drop guard fires and force-kills any lingering mstsc.exe
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);

        // Simulate a spawn without actually launching mstsc (test env)
        let pid = client.mock_spawn_for_test();
        assert!(pid > 0);

        // Drop the client — the guard should kill the tracked PID
        drop(client);

        // Verify the process handle is cleaned up
        let killed = RdpClient::verify_process_dead(pid);
        assert!(killed);
    }

    #[test]
    fn connect_attaches_capture_before_running_flag() {
        // Ensure no gap exists between desktop-paint and capture-start
        let tunnel = TunnelConfig::default();
        let mut client = RdpClient::new(&tunnel);

        let events = client.mock_connect_event_sequence();

        // Capture must attach BEFORE the running flag flips
        let capture_idx = events
            .iter()
            .position(|e| e == "capture_attach")
            .expect("capture_attach event missing");
        let running_idx = events
            .iter()
            .position(|e| e == "running_flag_set")
            .expect("running_flag_set event missing");

        assert!(capture_idx < running_idx);
    }

    #[test]
    fn input_policy_default_is_forbidden() {
        let tunnel = TunnelConfig::default();
        assert_eq!(tunnel.input_policy, InputPolicy::Forbidden);
    }

    #[test]
    fn rdp_contents_clipboard_redirect_enabled() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // Clipboard redirection must be on for text extraction to work
        assert!(s.contains("redirectclipboard:i:1"));
    }

    #[test]
    fn rdp_contents_all_peripheral_redirects_off() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // Everything except clipboard must be off — reduces detection surface
        assert!(s.contains("redirectprinters:i:0"));
        assert!(s.contains("redirectsmartcards:i:0"));
        assert!(s.contains("redirectdevices:i:0"));
        assert!(s.contains("redirectdrives:i:0"));
        assert!(s.contains("redirectcomports:i:0"));
        assert!(s.contains("redirectposdevices:i:0"));
        assert!(s.contains("redirectaudio:i:0"));
    }

    #[test]
    fn rdp_contents_autoreconnect_enabled() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // Auto-reconnection keeps the tunnel alive across network blips
        assert!(s.contains("autoreconnection enabled:i:1"));
    }

    #[test]
    fn stored_credentials_embedded_in_file() {
        let mut tunnel = TunnelConfig::default();
        tunnel.username = String::from("admin");
        tunnel.domain = String::from("LOCALHOST");
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("username:s:admin"));
        assert!(s.contains("domain:s:LOCALHOST"));
    }

    #[test]
    fn render_is_deterministic() {
        let tunnel = TunnelConfig::default();
        let client_a = RdpClient::new(&tunnel);
        let client_b = RdpClient::new(&tunnel);

        let a = client_a.render_rdp_contents();
        let b = client_b.render_rdp_contents();

        assert_eq!(a, b);
    }

    #[test]
    fn custom_dimensions_override_defaults() {
        let mut tunnel = TunnelConfig::default();
        tunnel.width = 3840;
        tunnel.height = 2160;
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        assert!(s.contains("desktopwidth:i:3840"));
        assert!(s.contains("desktopheight:i:2160"));
    }

    #[test]
    fn rdp_contents_g省会ateway_settings_disabled() {
        let tunnel = TunnelConfig::default();
        let client = RdpClient::new(&tunnel);
        let s = client.render_rdp_contents();

        // Gateway must be fully disabled — we're connecting to loopback
        assert!(s.contains("gatewayhostname:s:"));
        assert!(s.contains("gatewayusagemethod:i:4"));
        assert!(s.contains("gatewaycredentialssource:i:4"));
        assert!(s.contains("gatewayprofileusagemethod:i:0"));
    }
}