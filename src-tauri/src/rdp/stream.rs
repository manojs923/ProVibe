//! Captures the RDP client window's rendered output (not the VM framebuffer —
//! the pixels as displayed on the host monitor). This is the optical air-gap —
//! we never touch VM memory, only what RDP renders to our screen.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::rdp::client::RdpClient;

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, RECT};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_MODE_ROTATION_IDENTITY,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, GetClientRect, GetWindowRect,
    MapWindowPoints, GetWindowTextW,
};
#[cfg(windows)]
use windows::core::{Interface, HRESULT};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum StreamError {
    NoRdpWindow,
    BackendInit(String),
    DuplicationUnsupported,
    FrameTimeout,
    WindowGone,
    GdiFailure(String),
    ChannelClosed,
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::NoRdpWindow => write!(f, "no mstsc.exe top-level window found"),
            StreamError::BackendInit(s) => write!(f, "capture backend failed to init: {}", s),
            StreamError::DuplicationUnsupported => {
                write!(f, "DXGI Desktop Duplication unavailable on this system")
            }
            StreamError::FrameTimeout => write!(f, "duplicate frame acquisition timed out"),
            StreamError::WindowGone => write!(f, "target window was closed mid-capture"),
            StreamError::GdiFailure(s) => write!(f, "GDI fallback failed: {}", s),
            StreamError::ChannelClosed => write!(f, "frame receiver dropped"),
        }
    }
}

impl std::error::Error for StreamError {}

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

/// A single captured frame in tightly-packed BGRA, cropped to the RDP window.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Row stride in bytes (width * 4 when tightly packed).
    pub stride: u32,
    pub data: Vec<u8>,
    /// Host monotonic timestamp of acquisition, milliseconds since UNIX epoch.
    pub timestamp_ms: u128,
    /// Monotonically increasing frame counter across the stream's lifetime.
    pub sequence: u64,
}

impl Frame {
    /// Byte length of a tightly packed BGRA buffer for the given dimensions.
    pub fn buffer_size(width: u32, height: u32) -> usize {
        (width as usize) * (height as usize) * 4
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaptureBackend {
    /// DXGI Desktop Duplication — GPU-side, grabs the monitor scanout.
    /// Highest fidelity, matches exactly what the host monitor displays.
    DesktopDuplication,
    /// GDI BitBlt fallback — works everywhere, slight color rounding.
    GdiBlt,
}

#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub target_fps: u32,
    pub backend: CaptureBackend,
    /// Crop the duplicated desktop down to the RDP window's bounds.
    pub crop_to_window: bool,
    /// Optional downscale, e.g. (1280, 720) for bandwidth-constrained consumers.
    pub downscale: Option<(u32, u32)>,
    /// Milliseconds to wait for a new desktop frame before emitting a repeat.
    pub acquire_timeout_ms: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            target_fps: 30,
            backend: CaptureBackend::DesktopDuplication,
            crop_to_window: true,
            downscale: None,
            acquire_timeout_ms: 100,
        }
    }
}

impl StreamConfig {
    /// Frame interval derived from target fps.
    pub fn frame_interval(&self) -> Duration {
        if self.target_fps == 0 {
            return Duration::ZERO;
        }
        Duration::from_nanos(1_000_000_000 / self.target_fps as u64)
    }
}

// ---------------------------------------------------------------------------
// Window locator
// ---------------------------------------------------------------------------

/// Intersect a window rect with the desktop duplication output bounds and
/// clamp the result so the crop never reads out of the acquired surface.
pub fn compute_crop_rect(
    window_rect: (i32, i32, i32, i32),
    output_origin: (i32, i32),
    output_size: (u32, u32),
) -> (u32, u32, u32, u32) {
    // (x, y, w, h) relative to output origin
    let wx = window_rect.0 - output_origin.0;
    let wy = window_rect.1 - output_origin.1;
    let ww = (window_rect.2 - window_rect.0).max(0);
    let wh = (window_rect.3 - window_rect.1).max(0);

    // Clamp left/top into the output
    let cx = wx.clamp(0, output_size.0 as i32) as u32;
    let cy = wy.clamp(0, output_size.1 as i32) as u32;

    // Clamp width/height so cx+cw <= output width
    let max_w = output_size.0.saturating_sub(cx);
    let max_h = output_size.1.saturating_sub(cy);
    let cw = (ww as u32).saturating_sub(cx.saturating_sub(wx.max(0) as u32)).min(max_w);
    let ch = (wh as u32).saturating_sub(cy.saturating_sub(wy.max(0) as u32)).min(max_h);

    (cx, cy, cw, ch)
}

/// Extract a sub-rectangle from a tightly packed BGRA source buffer.
/// Returns a new tightly packed buffer of (w, h).
pub fn crop_bgra(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    crop: (u32, u32, u32, u32),
) -> Vec<u8> {
    let (cx, cy, cw, ch) = crop;
    if cw == 0 || ch == 0 {
        return Vec::new();
    }
    // Safety clamp — never read outside the source.
    let cw = cw.min(src_width.saturating_sub(cx));
    let ch = ch.min(src_height.saturating_sub(cy));

    let mut dst = Vec::with_capacity(Frame::buffer_size(cw, ch));
    for row in 0..ch {
        let src_start = ((cy + row) as usize) * (src_width as usize) * 4 + (cx as usize) * 4;
        let src_end = src_start + (cw as usize) * 4;
        if src_end <= src.len() {
            dst.extend_from_slice(&src[src_start..src_end]);
        }
    }
    dst
}

/// Nearest-neighbor downscale of a tightly packed BGRA buffer.
pub fn downscale_bgra(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return Vec::new();
    }
    if dst_w == src_w && dst_h == src_h {
        return src.to_vec();
    }
    let mut dst = Vec::with_capacity(Frame::buffer_size(dst_w, dst_h));
    for dy in 0..dst_h {
        // Precompute source row: ((dy * src_h) / dst_h)
        let sy = ((dy as u64 * src_h as u64) / dst_w.max(1) as u64 / dst_h as u64 * dst_h as u64
            / dst_h as u64) as u32;
        let sy = ((dy as u64 * src_h as u64) / dst_h as u64) as u32;
        for dx in 0..dst_w {
            let sx = ((dx as u64 * src_w as u64) / dst_w as u64) as u32;
            let si = (sy as usize * src_w as usize + sx as usize) * 4;
            if si + 4 <= src.len() {
                dst.extend_from_slice(&src[si..si + 4]);
            }
        }
    }
    dst
}

#[cfg(windows)]
struct EnumState {
    target_pid: u32,
    found: Option<isize>,
}

#[cfg(windows)]
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: isize) -> i32 {
    let state = &mut *(lparam as *mut EnumState);
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == state.target_pid && IsWindowVisible(hwnd).as_bool() {
        // Skip tiny helper windows — require a plausible client area.
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w >= 400 && h >= 300 {
            // Prefer windows with a title (the main session window).
            let mut title = [0u16; 64];
            let len = GetWindowTextW(hwnd, &mut title);
            if len > 0 || state.found.is_none() {
                state.found = Some(hwnd.0 as isize);
            }
        }
    }
    1 // continue enumeration
}

/// Locate the top-level visible window belonging to the RDP client process.
#[cfg(windows)]
pub fn find_rdp_window(process_id: u32) -> Result<HWND, StreamError> {
    let mut state = EnumState {
        target_pid: process_id,
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), &mut state as *mut EnumState as isize);
    }
    state
        .found
        .map(|h| HWND(h as *mut _))
        .ok_or(StreamError::NoRdpWindow)
}

/// Window rectangle in screen coordinates.
#[cfg(windows)]
pub fn window_screen_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        (rect.left, rect.top, rect.right, rect.bottom)
    }
}

// ---------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------

pub struct RdpStream {
    config: StreamConfig,
    running: Arc<AtomicBool>,
    sequence: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<StreamError>>>,
    #[allow(dead_code)]
    rdp_client_pid: Option<u32>,
}

impl RdpStream {
    pub fn new(config: StreamConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            sequence: Arc::new(AtomicU64::new(0)),
            worker: None,
            last_error: Arc::new(Mutex::new(None)),
            rdp_client_pid: None,
        }
    }

    /// Attach to a running RdpClient's spawned mstsc.exe, locate its window,
    /// and begin capturing. Returns a receiver of cropped frames.
    #[cfg(windows)]
    pub fn start_attached(
        mut self,
        client: &RdpClient,
    ) -> Result<(Self, std::sync::mpsc::Receiver<Frame>), StreamError> {
        let pid = client.process_id().ok_or(StreamError::NoRdpWindow)?;
        self.rdp_client_pid = Some(pid);
        let hwnd = find_rdp_window(pid)?;
        self.start_hwnd(hwnd)
    }

    /// Begin capturing a specific window handle.
    pub fn start_hwnd(
        mut self,
        #[cfg(windows)] hwnd: HWND,
    ) -> Result<(Self, std::sync::mpsc::Receiver<Frame>), StreamError> {
        let (tx, rx) = std::sync::mpsc::channel::<Frame>();
        self.running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.running);
        let sequence = Arc::clone(&self.sequence);
        let last_error = Arc::clone(&self.last_error);
        let config = self.config.clone();

        let worker = thread::Builder::new()
            .name("rdp-capture".to_string())
            .spawn(move || {
                let frame_interval = Duration::from_secs_f64(1.0 / config.target_fps.max(1.0).max(0.1) as f64);
                let mut next_frame_at = Instant::now();

                #[cfg(windows)]
                let mut gdi_fallback: Option<(HDC, HDC, HBITMAP, BITMAPINFO, Vec<u8>)> = None;

                loop {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }

                    let now = Instant::now();
                    if now < next_frame_at {
                        thread::sleep(next_frame_at - now);
                    }
                    next_frame_at = Instant::now() + frame_interval;

                    // Acquire raw frame using the configured backend
                    let raw = match config.backend {
                        #[cfg(windows)]
                        CaptureBackend::DesktopDuplication => {
                            match Self::acquire_dda(hwnd, &config) {
                                Ok(f) => f,
                                Err(e) => {
                                    // DDA can fail if the window is minimized or occluded
                                    // Fall back to GDI capture in that case
                                    *last_error.lock().unwrap() = Some(e);
                                    match Self::acquire_gdi(hwnd, &config, &mut gdi_fallback) {
                                        Ok(f) => f,
                                        Err(e2) => {
                                            *last_error.lock().unwrap() = Some(e2);
                                            thread::sleep(Duration::from_millis(100));
                                            continue;
                                        }
                                    }
                                }
                            }
                        }
                        #[cfg(windows)]
                        CaptureBackend::Gdi => {
                            match Self::acquire_gdi(hwnd, &config, &mut gdi_fallback) {
                                Ok(f) => f,
                                Err(e) => {
                                    *last_error.lock().unwrap() = Some(e);
                                    thread::sleep(Duration::from_millis(100));
                                    continue;
                                }
                            }
                        }
                    };

                    let frame = Frame {
                        width: raw.1,
                        height: raw.2,
                        stride: raw.3,
                        data: raw.0,
                        timestamp: SystemTime::now(),
                        sequence: sequence.fetch_add(1, Ordering::SeqCst),
                    };

                    if tx.send(frame).is_err() {
                        // Receiver dropped, stop the loop
                        running.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            })
            .map_err(|e| StreamError::ThreadSpawn(e.to_string()))?;

        self.worker_handle = Some(worker);
        self.hwnd = Some(hwnd);
        Ok((self, rx))
    }

    #[cfg(windows)]
    fn acquire_dda(
        hwnd: HWND,
        config: &StreamConfig,
    ) -> Result<(Vec<u8>, u32, u32, usize), StreamError> {
        use winapi::um::dxgi1_2::{IDXGIResource, DXGI_OUTDUPL_DESC};
        use winapi::um::dxgi::{IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication};
        use winapi::Interface;

        // Get the window rect to determine which monitor it's on
        let mut rect = win32::Win32Rect::default();
        if unsafe { win32::ffi::GetWindowRect(hwnd, &mut rect) } == 0 {
            return Err(StreamError::WindowNotFound);
        }

        // Determine which output (monitor) the window is primarily on
        let center_x = (rect.left + rect.right) / 2;
        let center_y = (rect.top + rect.bottom) / 2;

        unsafe {
            // Enumerate outputs to find the one containing the window center
            let mut output_index: u32 = 0;
            let mut target_output: Option<*mut IDXGIOutput1> = None;

            // ... (DXGI enumeration code here, selecting output by position)

            // Acquire next frame with timeout
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: *mut IDXGIResource = std::ptr::null_mut();

            let hr = (*duplication).AcquireNextFrame(
                config.acquire_timeout_ms,
                &mut frame_info,
                &mut resource,
            );

            if hr != winapi::shared::winerror::S_OK {
                if hr == winapi::shared::winerror::DXGI_ERROR_WAIT_TIMEOUT {
                    return Err(StreamError::Timeout);
                }
                return Err(StreamError::CaptureFailed(format!("AcquireNextFrame hr=0x{:08X}", hr)));
            }

            // Map the desktop texture, crop to window rect, and copy pixels
            // ... (texture mapping and pixel copy code)

            (*resource).Release();
            (*duplication).ReleaseFrame();

            Ok((pixel_data, width, height, stride))
        }
    }

    #[cfg(windows)]
    fn acquire_gdi(
        hwnd: HWND,
        config: &StreamConfig,
        state: &mut Option<(HDC, HDC, HBITMAP, BITMAPINFO, Vec<u8>)>,
    ) -> Result<(Vec<u8>, u32, u32, usize), StreamError> {
        unsafe {
            let mut rect = win32::Win32Rect::default();
            if win32::ffi::GetWindowRect(hwnd, &mut rect) == 0 {
                return Err(StreamError::WindowNotFound);
            }

            let width = (rect.right - rect.left).max(1) as u32;
            let height = (rect.bottom - rect.top).max(1) as u32;
            let stride = (width * 4) as usize; // BGRA, 4 bytes per pixel

            // Lazily create or resize GDI resources if dimensions changed
            if state.is_none() || {
                let (_, _, _, bi, _) = state.as_ref().unwrap();
                bi.bmiHeader.biWidth != width as i32 || bi.bmiHeader.biHeight != -(height as i32)
            } {
                if let Some((_, mem_dc, _, _, _)) = state.take() {
                    win32::ffi::DeleteDC(mem_dc);
                }

                let hwnd_dc = win32::ffi::GetDC(hwnd);
                let mem_dc = win32::ffi::CreateCompatibleDC(hwnd_dc);
                let bmp = win32::ffi::CreateCompatibleBitmap(hwnd_dc, width as i32, height as i32);
                win32::ffi::ReleaseDC(hwnd, hwnd_dc);

                let mut bi = std::mem::zeroed::<BITMAPINFO>();
                bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bi.bmiHeader.biWidth = width as i32;
                bi.bmiHeader.biHeight = -(height as i32); // top-down
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                bi.bmiHeader.biCompression = win32::ffi::BI_RGB;

                let buf = vec![0u8; stride * height as usize];
                win32::ffi::SelectObject(mem_dc, bmp as *mut _);

                *state = Some((hwnd_dc, mem_dc, bmp, bi, buf));
            }

            let (hwnd_dc, mem_dc, bmp, bi, ref mut buf) = state.as_mut().unwrap();

            // Use PrintWindow with PW_RENDERFULLCONTENT for windows that may be occluded
            win32::ffi::PrintWindow(hwnd, *mem_dc, 3 /* PW_RENDERFULLCONTENT */);

            // Copy pixels from the memory DC
            let hwnd_dc_real = win32::ffi::GetDC(hwnd);
            win32::ffi::GetDIBits(
                hwnd_dc_real,
                *bmp,
                0,
                height as u32,
                buf.as_mut_ptr() as *mut _,
                bi as *mut BITMAPINFO,
                win32::ffi::DIB_RGB_COLORS,
            );
            win32::ffi::ReleaseDC(hwnd, hwnd_dc_real);

            Ok((buf.clone(), width, height, stride))
        }
    }