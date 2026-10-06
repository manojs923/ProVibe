//! Captures the RDP client window's rendered output (not the VM framebuffer —
//! the pixels as displayed on the host monitor). This is the optical air-gap —
//! we never touch VM memory, only what RDP renders to our screen.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::mpsc;

use crate::rdp::client::RdpClient;

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM, BOOL, RECT, POINT};


#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    IDXGIOutput, IDXGIDevice, IDXGIFactory1,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_MODE_ROTATION_IDENTITY,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextW, IsWindowVisible,
    GetClientRect, GetForegroundWindow,
    SetForegroundWindow, FindWindowW,
};

use windows::Win32::UI::WindowsAndMessaging::PrintWindow;
use windows::Win32::Graphics::Gdi::{
    MapWindowPoints, HDC, HBITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
    GetDC, GetDIBits, ReleaseDC, SelectObject, BitBlt, SRCCOPY, DIB_RGB_COLORS,
};

#[cfg(windows)]
use windows::core::{Interface, HRESULT};

use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("RDP window not found")]
    NoRdpWindow,
    #[error("backend init failed: {0}")]
    BackendInit(String),
    #[error("desktop duplication unsupported")]
    DuplicationUnsupported,
    #[error("frame timeout")]
    FrameTimeout,
    #[error("window gone")]
    WindowGone,
    #[error("GDI failure: {0}")]
    GdiFailure(String),
    #[error("channel closed")]
    ChannelClosed,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

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
    DesktopDuplication,
    Gdi,
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
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut EnumState);
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
    BOOL(1)
}

/// Locate the top-level visible window belonging to the RDP client process.
#[cfg(windows)]
pub fn find_rdp_window(process_id: u32) -> Result<HWND, StreamError> {
    let mut state = EnumState {
        target_pid: process_id,
        found: None,
    };
    unsafe {
        // Add this import at the top if missing: use windows::Win32::Foundation::LPARAM;
        let _ = EnumWindows(Some(enum_proc), windows::Win32::Foundation::LPARAM(&mut state as *mut _ as isize));
    }
    state
        .found
        .map(|h| HWND(h as *mut core::ffi::c_void))
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
    frames: std::sync::mpsc::Receiver<Frame>,
    sequence: Arc<AtomicU64>,
    worker_handle: Option<JoinHandle<()>>,
    hwnd: HWND,
    running: Arc<AtomicBool>,
    rdp_client_pid: Option<u32>,
    last_error: Arc<Mutex<Option<String>>>,
    config: StreamConfig,
}   

impl RdpStream {
    pub fn new(config: StreamConfig) -> Self {
       Self {
    frames: rx,
    sequence: Arc::new(AtomicU64::new(0)),
    worker_handle: None,
    hwnd: None,
    running: Arc::new(AtomicBool::new(false)),
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
        let sequence = self.sequence.clone();
        let last_error = Arc::clone(&self.last_error);
        let config = self.config.clone();

        let worker = thread::Builder::new()
            .name("rdp-capture".to_string())
            .spawn(move || {
                let fps = config.target_fps.max(1) as f64;
                let frame_interval = Duration::from_secs_f64(1.0 / fps.max(0.1));
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
                                    *last_error.lock().unwrap() = Some(e.to_string());
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
                                    *last_error.lock().unwrap() = Some(e.to_string());
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
                        timestamp_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
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
        // use winapi::um::dxgi1_2::{IDXGIResource, DXGI_OUTDUPL_DESC};
        // use winapi::um::dxgi::{IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication};
        // use winapi::Interface;   -> i have commented it 

        // Get the window rect to determine which monitor it's on
        let mut rect = windows::Win32::Foundation::RECT::default();
        if unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect) }.is_err() {
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
    let mut resource: Option<IDXGIResource> = None;
    
    let hr = duplication.AcquireNextFrame(100, &mut frame_info, &mut resource);
    
    if hr.is_err() {
        return Err(StreamError::FrameTimeout);
    }
    
    if let Some(resource) = resource {
        // Do something with resource
        drop(resource);
    }
    
    duplication.ReleaseFrame();
    
    // Define your variables here from frame_info or other sources
    let pixel_data: Vec<u8> = Vec::new(); // Replace with actual data extraction
    let width: u32 = frame_info.LastPresentTime.QuadPart as u32; // Example, adjust as needed
    let height: u32 = 0; // Replace with actual value
    let stride: u32 = 0; // Replace with actual value
    
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
            let mut rect = windows::Win32::Foundation::RECT::default();
            
            // GetWindowRect now returns a Result instead of an integer
            if windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect).is_err() {
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
                    windows::Win32::Graphics::Gdi::DeleteDC(mem_dc);
                }

                let hwnd_dc = windows::Win32::Graphics::Gdi::GetDC(hwnd);
                let mem_dc = windows::Win32::Graphics::Gdi::CreateCompatibleDC(hwnd_dc);
                let bmp = windows::Win32::Graphics::Gdi::CreateCompatibleBitmap(hwnd_dc, width as i32, height as i32);
                windows::Win32::Graphics::Gdi::ReleaseDC(hwnd, hwnd_dc);

                let mut bi = std::mem::zeroed::<windows::Win32::Graphics::Gdi::BITMAPINFO>();
                bi.bmiHeader.biSize = std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
                bi.bmiHeader.biWidth = width as i32;
                bi.bmiHeader.biHeight = -(height as i32); // top-down
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                bi.bmiHeader.biCompression = windows::Win32::Graphics::Gdi::BI_RGB.0;

                let buf = vec![0u8; stride * height as usize];
                
                // SelectObject automatically accepts HBITMAP in the modern crate
                windows::Win32::Graphics::Gdi::SelectObject(mem_dc, bmp);

                *state = Some((hwnd_dc, mem_dc, bmp, bi, buf));
            }

            let (hwnd_dc, mem_dc, bmp, bi, ref mut buf) = state.as_mut().unwrap();

            // Use PrintWindow with PW_RENDERFULLCONTENT for windows that may be occluded
            PrintWindow(hwnd, *mem_dc, 3);

            // Copy pixels from the memory DC
            let hwnd_dc_real = windows::Win32::Graphics::Gdi::GetDC(hwnd);
            windows::Win32::Graphics::Gdi::GetDIBits(
                hwnd_dc_real,
                *bmp,
                0,
                height as u32,
                Some(buf.as_mut_ptr() as *mut _), // Wrap in Some() for Option<*mut c_void>
                bi as *mut windows::Win32::Graphics::Gdi::BITMAPINFO,
                windows::Win32::Graphics::Gdi::DIB_RGB_COLORS,
            );
            windows::Win32::Graphics::Gdi::ReleaseDC(hwnd, hwnd_dc_real);

            Ok((buf.clone(), width, height, stride))
        }
    }
}

fn capture_dxgi_frame(
    duplication: &IDXGIOutputDuplication,
    width: &mut u32,
    height: &mut u32,
) -> Result<(Vec<u8>, u32), StreamError> {
    let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;

    unsafe {
        duplication
            .AcquireNextFrame(100, &mut frame_info, &mut resource)
            .map_err(|e| StreamError::FrameTimeout)?;
    }

    let texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D = resource
        .and_then(|r| r.cast().ok())
        .ok_or(StreamError::CaptureFailed("resource cast failed".into()))?;

    let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
    }

    *width = desc.Width;
    *height = desc.Height;
    let stride = desc.Width * 4;

    let mut pixel_data = vec![0u8; (stride * desc.Height) as usize];

    unsafe {
        duplication.ReleaseFrame().ok();
    }

    Ok((pixel_data, stride))
}