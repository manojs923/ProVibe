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
///   new()      -> resolve paths, stage credentials, nothing on screen yet
///   spawn()    -> write .rdp, launch mstsc /f, take ownership of Child
///   wait_for_desktop() -> poll until the live-session window exists
///   kill()     -> terminate child + orphan sweep
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
    ///   - authentication level:i:0 -> no server cert warnings
    ///   - enablecredsspsupport:i:0 -> don't require network-level auth
    ///   - prompt for credentials:i:0 -> trust the stored cmdkey entry
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
                    0x0112,          // WM_SYSCOMMAND
                    WPARAM(0xF060),  // SC_CLOSE
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
