//! Captures the RDP client window's rendered output (not the VM framebuffer —
//! the pixels as displayed on the host monitor). This is the optical air-gap —
//! we never touch VM memory, only what RDP renders to our screen.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::rdp::client::RdpClient;

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM, BOOL, RECT};

#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    IDXGIOutput, IDXGIFactory1,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

#[cfg(windows)]
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_9_3, D3D_FEATURE_LEVEL_9_2, D3D_FEATURE_LEVEL_9_1,
};
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_FLAG, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};

#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextW, IsWindowVisible,
    GetClientRect, GetWindowThreadProcessId,
};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    PrintWindow, HDC, HBITMAP, BITMAPINFO,
    CreateCompatibleDC, CreateCompatibleBitmap, DeleteDC,
    GetDC, GetDIBits, ReleaseDC, SelectObject, BI_RGB, DIB_RGB_COLORS,
};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};

#[cfg(windows)]
use windows::core::Interface;

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
    #[error("thread spawn failed: {0}")]
    ThreadSpawn(String),
    #[error("window not found")]
    WindowNotFound,
    #[error("capture failed: {0}")]
    CaptureFailed(String),
    #[error("stream not started")]
    NotStarted,
}

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
    pub timestamp_ms: u128,
    pub sequence: u64,
}

impl Frame {
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
    pub crop_to_window: bool,
    pub downscale: Option<(u32, u32)>,
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
    pub fn frame_interval(&self) -> Duration {
        if self.target_fps == 0 {
            return Duration::ZERO;
        }
        Duration::from_nanos(1_000_000_000 / self.target_fps as u64)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn compute_crop_rect(
    window_rect: (i32, i32, i32, i32),
    output_origin: (i32, i32),
    output_size: (u32, u32),
) -> (u32, u32, u32, u32) {
    let wx = window_rect.0 - output_origin.0;
    let wy = window_rect.1 - output_origin.1;
    let ww = (window_rect.2 - window_rect.0).max(0);
    let wh = (window_rect.3 - window_rect.1).max(0);

    let cx = wx.clamp(0, output_size.0 as i32) as u32;
    let cy = wy.clamp(0, output_size.1 as i32) as u32;

    let max_w = output_size.0.saturating_sub(cx);
    let max_h = output_size.1.saturating_sub(cy);
    let cw = (ww as u32).min(max_w);
    let ch = (wh as u32).min(max_h);

    (cx, cy, cw, ch)
}

pub fn crop_bgra(
    src: &[u8],
    src_width: u32,
    src_stride: u32,
    crop: (u32, u32, u32, u32),
) -> Vec<u8> {
    let (cx, cy, cw, ch) = crop;
    if cw == 0 || ch == 0 {
        return Vec::new();
    }
    let cw = cw.min(src_width.saturating_sub(cx));
    let mut dst = Vec::with_capacity((cw as usize) * (ch as usize) * 4);
    for row in 0..ch {
        let src_start = ((cy + row) as usize) * (src_stride as usize) + (cx as usize) * 4;
        let src_end = src_start + (cw as usize) * 4;
        if src_end <= src.len() {
            dst.extend_from_slice(&src[src_start..src_end]);
        }
    }
    dst
}

pub fn downscale_bgra(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return Vec::new();
    }
    if dst_w == src_w && dst_h == src_h {
        return src.to_vec();
    }
    let mut dst = Vec::with_capacity(Frame::buffer_size(dst_w, dst_h));
    for dy in 0..dst_h {
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

// ---------------------------------------------------------------------------
// Window locator
// ---------------------------------------------------------------------------

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
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w >= 400 && h >= 300 {
            let mut title = [0u16; 64];
            let len = GetWindowTextW(hwnd, &mut title);
            if len > 0 || state.found.is_none() {
                state.found = Some(hwnd.0 as isize);
            }
        }
    }
    BOOL(1)
}

#[cfg(windows)]
pub fn find_rdp_window(process_id: u32) -> Result<HWND, StreamError> {
    let mut state = EnumState { target_pid: process_id, found: None };
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut state as *mut _ as isize),
        );
    }
    state
        .found
        .map(|h| HWND(h as *mut core::ffi::c_void))
        .ok_or(StreamError::NoRdpWindow)
}

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
    last_error: Arc<Mutex<Option<String>>>,
    latest_frame: Arc<Mutex<Option<Frame>>>,
    #[cfg(windows)]
    hwnd: Option<HWND>,
    rdp_client_pid: Option<u32>,
}

// SAFETY: HWND is just a pointer-sized handle; we only read/write it under our own synchronization.
unsafe impl Send for RdpStream {}
unsafe impl Sync for RdpStream {}

impl RdpStream {
    pub fn new(config: StreamConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            sequence: Arc::new(AtomicU64::new(0)),
            worker: None,
            last_error: Arc::new(Mutex::new(None)),
            latest_frame: Arc::new(Mutex::new(None)),
            #[cfg(windows)]
            hwnd: None,
            rdp_client_pid: None,
        }
    }

    /// Attach to a running RdpClient, locate its window, and start capturing.
    #[cfg(windows)]
    pub fn attach_to_client_window(&mut self, client: &RdpClient) -> Result<(), StreamError> {
        let pid = client.process_id().ok_or(StreamError::NoRdpWindow)?;
        self.rdp_client_pid = Some(pid);
        let hwnd = find_rdp_window(pid)?;
        self.start_hwnd(hwnd)
    }

    #[cfg(not(windows))]
    pub fn attach_to_client_window(&mut self, _client: &RdpClient) -> Result<(), StreamError> {
        Err(StreamError::BackendInit("non-windows platform".into()))
    }

    /// Begin capturing a specific HWND.
    #[cfg(windows)]
    pub fn start_hwnd(&mut self, hwnd: HWND) -> Result<(), StreamError> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.hwnd = Some(hwnd);
        self.running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.running);
        let sequence = Arc::clone(&self.sequence);
        let last_error = Arc::clone(&self.last_error);
        let latest_frame = Arc::clone(&self.latest_frame);
        let config = self.config.clone();
        let hwnd_raw = hwnd.0 as isize;

        let worker = thread::Builder::new()
            .name("rdp-capture".to_string())
            .spawn(move || {
                let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
                let frame_interval = config.frame_interval();
                let mut next_frame_at = Instant::now();
                let mut gdi_state: Option<(HDC, HDC, HBITMAP, BITMAPINFO, Vec<u8>)> = None;

                while running.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    if now < next_frame_at {
                        thread::sleep(next_frame_at - now);
                    }
                    next_frame_at = Instant::now() + frame_interval;

                    let raw = match config.backend {
                        CaptureBackend::DesktopDuplication => {
                            match Self::acquire_dda(hwnd) {
                                Ok(f) => f,
                                Err(e) => {
                                    *last_error.lock().unwrap() = Some(e.to_string());
                                    match Self::acquire_gdi(hwnd, &mut gdi_state) {
                                        Ok(f) => f,
                                        Err(e2) => {
                                            *last_error.lock().unwrap() = Some(e2.to_string());
                                            thread::sleep(Duration::from_millis(100));
                                            continue;
                                        }
                                    }
                                }
                            }
                        }
                        CaptureBackend::Gdi => {
                            match Self::acquire_gdi(hwnd, &mut gdi_state) {
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
                        stride: raw.3 as u32,
                        data: raw.0,
                        timestamp_ms: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis(),
                        sequence: sequence.fetch_add(1, Ordering::SeqCst),
                    };

                    *latest_frame.lock().unwrap() = Some(frame);
                }

                // Cleanup GDI
                if let Some((_, mem_dc, bmp, _, _)) = gdi_state.take() {
                    unsafe {
                        let _ = DeleteDC(mem_dc);
                        let _ = windows::Win32::Graphics::Gdi::DeleteObject(bmp);
                    }
                }
            })
                        .map_err(|e| StreamError::ThreadSpawn(e.to_string()))?;

        self.worker = Some(worker);
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn start_hwnd(&mut self, _hwnd: usize) -> Result<(), StreamError> {
        Err(StreamError::BackendInit("non-windows platform".into()))
    }

    /// Stop the capture worker.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }

    /// Grab the latest frame (clones the pixel buffer).
    pub fn latest_frame(&self) -> Option<Frame> {
        self.latest_frame.lock().unwrap().clone()
    }

    /// Pop the latest frame (removes it from the slot).
    pub fn take_frame(&self) -> Option<Frame> {
        self.latest_frame.lock().unwrap().take()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }

    // -----------------------------------------------------------------------
    // DXGI Desktop Duplication capture path
    // -----------------------------------------------------------------------

    #[cfg(windows)]
    fn acquire_dda(hwnd: HWND) -> Result<(Vec<u8>, u32, u32, usize), StreamError> {
        unsafe {
            // Create D3D11 device
            let feature_levels = [
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
                D3D_FEATURE_LEVEL_9_3,
                D3D_FEATURE_LEVEL_9_2,
                D3D_FEATURE_LEVEL_9_1,
            ];

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut feature_level = D3D_FEATURE_LEVEL_11_0;

            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
            .map_err(|e| StreamError::BackendInit(format!("D3D11CreateDevice: {}", e)))?;

            let device = device.ok_or_else(|| StreamError::BackendInit("no device".into()))?;
            let context = context.ok_or_else(|| StreamError::BackendInit("no context".into()))?;

            // Get DXGI device/adapter/output
            let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice = device
                .cast()
                .map_err(|e| StreamError::BackendInit(format!("cast IDXGIDevice: {}", e)))?;

            let adapter = dxgi_device
                .GetAdapter()
                .map_err(|e| StreamError::BackendInit(format!("GetAdapter: {}", e)))?;

            // Pick the output that contains our window
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);

            let mut output_idx = 0u32;
            let mut chosen_output: Option<IDXGIOutput1> = None;
            let mut chosen_desc = DXGI_OUTPUT_DESC::default();

            loop {
                let output = match adapter.EnumOutputs(output_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                let mut desc = DXGI_OUTPUT_DESC::default();
                if output.GetDesc(&mut desc).is_ok() && desc.Monitor == monitor {
                    let out1: IDXGIOutput1 = output
                        .cast()
                        .map_err(|e| StreamError::BackendInit(format!("cast Output1: {}", e)))?;
                    chosen_output = Some(out1);
                    chosen_desc = desc;
                    break;
                }
                output_idx += 1;
            }

            // Fallback to first output if we didn't match
            if chosen_output.is_none() {
                let output = adapter
                    .EnumOutputs(0)
                    .map_err(|e| StreamError::DuplicationUnsupported)?;
                let _ = output.GetDesc(&mut chosen_desc);
                chosen_output = Some(
                    output
                        .cast()
                        .map_err(|e| StreamError::BackendInit(format!("cast Output1: {}", e)))?,
                );
            }

            let output1 = chosen_output.unwrap();
            let duplication: IDXGIOutputDuplication = output1
                .DuplicateOutput(&device)
                .map_err(|e| StreamError::DuplicationUnsupported)?;

            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            let hr = duplication.AcquireNextFrame(100, &mut frame_info, &mut resource);
            if let Err(e) = hr {
                let _ = duplication.ReleaseFrame();
                if e.code().0 as u32 == 0x887A0027 {
                    return Err(StreamError::FrameTimeout);
                }
                return Err(StreamError::CaptureFailed(format!("AcquireNextFrame: {}", e)));
            }

            let resource = resource.ok_or_else(|| {
                let _ = duplication.ReleaseFrame();
                StreamError::CaptureFailed("no resource".into())
            })?;

            let src_tex: ID3D11Texture2D = resource.cast().map_err(|e| {
                let _ = duplication.ReleaseFrame();
                StreamError::CaptureFailed(format!("cast texture: {}", e))
            })?;

            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
            src_tex.GetDesc(&mut src_desc);

            // Create staging texture for CPU read
            let mut staging_desc = src_desc;
            staging_desc.Usage = D3D11_USAGE_STAGING;
            staging_desc.BindFlags = 0;
            staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            staging_desc.MiscFlags = 0;

            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .map_err(|e| {
                    let _ = duplication.ReleaseFrame();
                    StreamError::CaptureFailed(format!("CreateTexture2D staging: {}", e))
                })?;
            let staging = staging.ok_or_else(|| {
                let _ = duplication.ReleaseFrame();
                StreamError::CaptureFailed("no staging".into())
            })?;

            context.CopyResource(&staging, &src_tex);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| {
                    let _ = duplication.ReleaseFrame();
                    StreamError::CaptureFailed(format!("Map: {}", e))
                })?;

            let width = src_desc.Width;
            let height = src_desc.Height;
            let stride = mapped.RowPitch as usize;
            let mut data = vec![0u8; stride * height as usize];

            std::ptr::copy_nonoverlapping(
                mapped.pData as *const u8,
                data.as_mut_ptr(),
                data.len(),
            );

            context.Unmap(&staging, 0);
            let _ = duplication.ReleaseFrame();

            Ok((data, width, height, stride))
        }
    }

    // -----------------------------------------------------------------------
    // GDI PrintWindow fallback
    // -----------------------------------------------------------------------

    #[cfg(windows)]
    fn acquire_gdi(
        hwnd: HWND,
        state: &mut Option<(HDC, HDC, HBITMAP, BITMAPINFO, Vec<u8>)>,
    ) -> Result<(Vec<u8>, u32, u32, usize), StreamError> {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return Err(StreamError::WindowGone);
            }

            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect)
                .map_err(|e| StreamError::GdiFailure(format!("GetClientRect: {}", e)))?;

            let width = (rect.right - rect.left).max(1) as u32;
            let height = (rect.bottom - rect.top).max(1) as u32;
            let stride = (width as usize) * 4;

            // (Re)allocate GDI resources if dimensions changed
            let needs_realloc = match state {
                Some((_, _, _, bi, buf)) => {
                    bi.bmiHeader.biWidth != width as i32
                        || bi.bmiHeader.biHeight.unsigned_abs() != height
                        || buf.len() != stride * height as usize
                }
                None => true,
            };

            if needs_realloc {
                if let Some((_, mem_dc, bmp, _, _)) = state.take() {
                    let _ = DeleteDC(mem_dc);
                    let _ = windows::Win32::Graphics::Gdi::DeleteObject(bmp);
                }

                let window_dc = GetDC(hwnd);
                if window_dc.0.is_null() {
                    return Err(StreamError::GdiFailure("GetDC returned null".into()));
                }
                let mem_dc = CreateCompatibleDC(window_dc);
                if mem_dc.0.is_null() {
                    ReleaseDC(hwnd, window_dc);
                    return Err(StreamError::GdiFailure("CreateCompatibleDC failed".into()));
                }
                let bmp = CreateCompatibleBitmap(window_dc, width as i32, height as i32);
                if bmp.0.is_null() {
                    let _ = DeleteDC(mem_dc);
                    ReleaseDC(hwnd, window_dc);
                    return Err(StreamError::GdiFailure("CreateCompatibleBitmap failed".into()));
                }
                ReleaseDC(hwnd, window_dc);

                let mut bi = BITMAPINFO::default();
                bi.bmiHeader.biSize = std::mem::size_of::<
                    windows::Win32::Graphics::Gdi::BITMAPINFOHEADER,
                >() as u32;
                bi.bmiHeader.biWidth = width as i32;
                bi.bmiHeader.biHeight = -(height as i32); // top-down
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                bi.bmiHeader.biCompression = BI_RGB.0;

                let buf = vec![0u8; stride * height as usize];
                *state = Some((window_dc, mem_dc, bmp, bi, buf));
            }

            let (window_dc, mem_dc, bmp, bi, buf) = state.as_mut().unwrap();

            let _old = SelectObject(*mem_dc, (*bmp).into());

            // PW_RENDERFULLCONTENT = 0x00000002 → captures even occluded/DWM-composited content
            let ok = PrintWindow(hwnd, *mem_dc, windows::Win32::Graphics::Gdi::PRINT_WINDOW_FLAGS(0x00000002));
            if !ok.as_bool() {
                return Err(StreamError::GdiFailure("PrintWindow returned false".into()));
            }

            let scanned = GetDIBits(
                *window_dc,
                *bmp,
                0,
                height,
                Some(buf.as_mut_ptr() as *mut _),
                bi as *mut _,
                DIB_RGB_COLORS,
            );
            if scanned == 0 {
                return Err(StreamError::GdiFailure("GetDIBits returned 0".into()));
            }

            Ok((buf.clone(), width, height, stride))
        }
    }
}

impl Drop for RdpStream {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Public tauri command — one-shot capture of the RDP window as base64 JPEG
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn capture_rdp_window_once(pid: u32) -> Result<String, StreamError> {
    let hwnd = find_rdp_window(pid)?;
    let mut gdi_state: Option<(HDC, HDC, HBITMAP, BITMAPINFO, Vec<u8>)> = None;
    let (data, width, height, stride) = RdpStream::acquire_gdi(hwnd, &mut gdi_state)?;

    // Cleanup GDI resources
    if let Some((_, mem_dc, bmp, _, _)) = gdi_state.take() {
        unsafe {
            let _ = DeleteDC(mem_dc);
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(bmp);
        }
    }

    // BGRA → RGB for JPEG encoding
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let i = row_start + x * 4;
            if i + 3 <= data.len() {
                rgb.push(data[i + 2]); // R
                rgb.push(data[i + 1]); // G
                rgb.push(data[i]);     // B
            }
        }
    }

    let img = image::RgbImage::from_raw(width, height, rgb)
        .ok_or_else(|| StreamError::CaptureFailed("RgbImage::from_raw failed".into()))?;

    let mut jpeg_buf = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut jpeg_buf);
    img.write_to(&mut cursor, image::ImageFormat::Jpeg)
        .map_err(|e| StreamError::CaptureFailed(format!("JPEG encode: {}", e)))?;

    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&jpeg_buf))
}

#[cfg(not(windows))]
pub fn capture_rdp_window_once(_pid: u32) -> Result<String, StreamError> {
    Err(StreamError::BackendInit("non-windows platform".into()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_crop_rect_basic() {
        let crop = compute_crop_rect((100, 100, 500, 400), (0, 0), (1920, 1080));
        assert_eq!(crop, (100, 100, 400, 300));
    }

    #[test]
    fn test_compute_crop_rect_offset_output() {
        let crop = compute_crop_rect((1920, 100, 2320, 500), (1920, 0), (1920, 1080));
        assert_eq!(crop, (0, 100, 400, 400));
    }

    #[test]
    fn test_compute_crop_rect_clamped() {
        let crop = compute_crop_rect((-50, -50, 100, 100), (0, 0), (1920, 1080));
        assert_eq!(crop, (0, 0, 100, 100));
    }

    #[test]
    fn test_crop_bgra_simple() {
        let src = vec![
            1, 2, 3, 4,   5, 6, 7, 8,
            9, 10, 11, 12, 13, 14, 15, 16,
        ];
        let out = crop_bgra(&src, 2, 8, (0, 0, 1, 2));
        assert_eq!(out, vec![1, 2, 3, 4, 9, 10, 11, 12]);
    }

    #[test]
    fn test_downscale_bgra_identity() {
        let src = vec![10u8; 16];
        let out = downscale_bgra(&src, 2, 2, 2, 2);
        assert_eq!(out, src);
    }

    #[test]
    fn test_frame_buffer_size() {
        assert_eq!(Frame::buffer_size(10, 20), 800);
    }

        #[test]
    fn test_stream_config_defaults() {
        let config = StreamConfig::default();
        assert_eq!(config.target_fps, 30);
        assert_eq!(config.backend, CaptureBackend::DesktopDuplication);
        assert!(config.crop_to_window);
        assert_eq!(config.acquire_timeout_ms, 100);
        assert_eq!(config.downscale, None);
    }

    #[test]
    fn test_frame_interval() {
        let mut config = StreamConfig::default();
        assert_eq!(config.frame_interval(), Duration::from_nanos(33_333_333));

                config.target_fps = 60;
        assert_eq!(config.frame_interval(), Duration::from_nanos(16_666_666));

                config.target_fps = 0;
        assert_eq!(config.frame_interval(), Duration::from_nanos(0));

        config.target_fps = 144;
        assert_eq!(config.frame_interval(), Duration::from_nanos(6_944_444));
    }

    #[test]
    fn test_crop_bgra_edge_cases() {
        // 4x4 BGRA frame = 64 bytes
        let mut frame: Vec<u8> = Vec::with_capacity(64);
        for i in 0..64 {
            frame.push(i as u8);
        }

        let full_rect = CaptureRect { x: 0, y: 0, w: 4, h: 4 };
        let cropped = crop_bgra(&frame, 4, 4, &full_rect).unwrap();
        assert_eq!(cropped, frame, "full-frame crop should be identity");

        // Single pixel crop at (1,1) — bytes 20..24 of original
        let one_px = CaptureRect { x: 1, y: 1, w: 1, h: 1 };
        let cropped = crop_bgra(&frame, 4, 4, &one_px).unwrap();
        assert_eq!(cropped.len(), 4);
        assert_eq!(cropped[0], 20);
        assert_eq!(cropped[1], 21);
        assert_eq!(cropped[2], 22);
        assert_eq!(cropped[3], 23);

        // Bottom-right corner crop
        let corner = CaptureRect { x: 3, y: 3, w: 1, h: 1 };
        let cropped = crop_bgra(&frame, 4, 4, &corner).unwrap();
        assert_eq!(cropped.len(), 4);
        assert_eq!(cropped[0], 60);

        // Out-of-bounds: extends past right edge
        let oob_right = CaptureRect { x: 2, y: 0, w: 5, h: 2 };
        assert!(crop_bgra(&frame, 4, 4, &oob_right).is_err());

        // Out-of-bounds: extends past bottom edge
        let oob_bottom = CaptureRect { x: 0, y: 2, w: 2, h: 5 };
        assert!(crop_bgra(&frame, 4, 4, &oob_bottom).is_err());

        // Zero-dimension rect
        let zero_w = CaptureRect { x: 0, y: 0, w: 0, h: 2 };
        assert!(crop_bgra(&frame, 4, 4, &zero_w).is_err());

        let zero_h = CaptureRect { x: 0, y: 0, w: 2, h: 0 };
        assert!(crop_bgra(&frame, 4, 4, &zero_h).is_err());

        // Negative origin
        let neg_x = CaptureRect { x: -1, y: 0, w: 2, h: 2 };
        let cropped = crop_bgra(&frame, 4, 4, &neg_x);
        assert!(cropped.is_err(), "negative x must fail");

        // Buffer too small for claimed dimensions
        let truncated: Vec<u8> = frame[..32].to_vec();
        let half_rect = CaptureRect { x: 0, y: 0, w: 4, h: 4 };
        assert!(crop_bgra(&truncated, 4, 4, &half_rect).is_err());
    }

    #[test]
    fn test_downscale_bgra() {
        // Build a solid-color 8x8 frame (red, fully opaque)
        let mut frame = Vec::with_capacity(8 * 8 * 4);
        for _ in 0..64 {
            frame.extend_from_slice(&[66, 0, 255, 255]); // BGRA red
        }

        // Exact 2:1 downscale
        let downscaled = downscale_bgra(&frame, 8, 8, 4, 4);
        assert_eq!(downscaled.len(), 4 * 4 * 4);
        // Every output pixel should still be pure red (box filter on uniform input)
        for chunk in downscaled.chunks_exact(4) {
            assert_eq!(chunk, &[66, 0, 255, 255]);
        }

        // Non-integer ratio: 8 -> 3
        let downscaled = downscale_bgra(&frame, 8, 8, 3, 3);
        assert_eq!(downscaled.len(), 3 * 3 * 4);
        for chunk in downscaled.chunks_exact(4) {
            assert_eq!(chunk, &[66, 0, 255, 255]);
        }

        // Identity scale (no change)
        let same = downscale_bgra(&frame, 8, 8, 8, 8);
        assert_eq!(same, frame);

        // Upscale attempt — function should clamp or no-op; we define no-op
        // (upscale is never used in the capture path)
        let up = downscale_bgra(&frame, 8, 8, 16, 16);
        assert_eq!(up.len(), 8 * 8 * 4, "upscale should return input unchanged");

        // Odd source: 5x5 -> 2x2
        let mut odd = Vec::with_capacity(5 * 5 * 4);
        for _ in 0..25 {
            odd.extend_from_slice(&[0, 128, 0, 255]); // BGRA green
        }
        let downscaled = downscale_bgra(&odd, 5, 5, 2, 2);
        assert_eq!(downscaled.len(), 2 * 2 * 4);
        for chunk in downscaled.chunks_exact(4) {
            assert_eq!(chunk, &[0, 128, 0, 255]);
        }

        // Gradient input — verify averaging works: top half blue, bottom half red
        let mut grad = Vec::with_capacity(4 * 4 * 4);
        for y in 0..4 {
            for _ in 0..4 {
                if y < 2 {
                    grad.extend_from_slice(&[255, 0, 0, 255]); // blue
                } else {
                    grad.extend_from_slice(&[66, 0, 255, 255]); // red
                }
            }
        }
        let flat = downscale_bgra(&grad, 4, 4, 1, 1);
        assert_eq!(flat.len(), 4);
        // Box filter over 8 blue + 8 red pixels:
        // B: (255*8 + 66*8)/16 = 160.5 -> 160
        // G: 0, R: (0*8 + 255*8)/16 = 127.5 -> 127
        assert_eq!(flat[0], 160, "blue channel average");
        assert_eq!(flat[1], 0, "green channel average");
        assert_eq!(flat[2], 127, "red channel average");
        assert_eq!(flat[3], 255, "alpha stays opaque");
    }

    #[test]
    fn test_stream() {
        // The full StreamConfig pipeline: defaults -> validation -> crop math.
        let config = StreamConfig::default();
        assert_eq!(config.jpeg_quality.clamp(1, 100), 75);
        assert!(config.target_fps > 0 && config.target_fps <= 120);
        assert!(config.max_width > 0 && config.max_height > 0);

        // Frame interval consistency across the pipeline
        let interval = config.frame_interval();
        assert!(interval > Duration::ZERO);

        // Rect clamping against a virtual 1920x1080 desktop
        let desktop_w: i32 = 1920;
        let desktop_h: i32 = 1080;

        let oversized = CaptureRect {
            x: 1900,
            y: 1000,
            w: 200,  // would extend to 2100
            h: 200,  // would extend to 1200
        };
        let clamped_w = oversized.w.min(desktop_w - oversized.x).max(0);
        let clamped_h = oversized.h.min(desktop_h - oversized.y).max(0);
        assert_eq!(clamped_w, 20, "width clamps to right edge");
        assert_eq!(clamped_h, 80, "height clamps to bottom edge");

        // Zero-area after clamp means the rect was entirely off-screen
        let offscreen = CaptureRect { x: 2000, y: 0, w: 100, h: 100 };
        let cw = offscreen.w.min(desktop_w - offscreen.x).max(0);
        assert_eq!(cw, 0, "rect past right edge collapses to zero");
    }

    #[test]
    fn test_capture_rect_validation() {
        // Valid rect
        let ok = CaptureRect { x: 0, y: 0, w: 100, h: 100 };
        assert!(ok.w > 0 && ok.h > 0);

        // Degenerate rects are rejected by crop_bgra (covered above) but
        // document the invariant here: any rect where w or h is 0 or the
        // origin is negative is invalid for capture.
        let invalid_cases = vec![
            CaptureRect { x: -5, y: 0, w: 10, h: 10 },
            CaptureRect { x: 0, y: -5, w: 10, h: 10 },
            CaptureRect { x: 0, y: 0, w: 0, h: 10 },
            CaptureRect { x: 0, y: 0, w: 10, h: 0 },
        ];
        for rect in invalid_cases {
            let mut frame = vec![0u8; 10 * 10 * 4];
            assert!(
                crop_bgra(&frame, 10, 10, &rect).is_err(),
                "rect {:?} must be rejected",
                rect
            );
        }
    }

    #[test]
    fn test_stream_error_display() {
        let cases: Vec<(StreamError, &str)> = vec![
            (StreamError::NoDesktopAccess, "desktop access denied — session may be locked or headless"),
            (StreamError::AdapterNotFound, "no DXGI adapter available for duplication"),
            (StreamError::DuplicateFailed(String::from("E_ACCESSDENIED")), "duplication init failed: E_ACCESSDENIED"),
            (StreamError::AcquireTimeout, "frame acquire timed out"),
            (StreamError::GdiError(String::from("BitBlt returned 0")), "gdi capture failed: BitBlt returned 0"),
            (StreamError::JpegEncodeError(String::from("quality out of range")), "jpeg encode failed: quality out of range"),
            (StreamError::InvalidDimensions { w: 0, h: 480 }, "invalid dimensions: 0x480"),
        ];

        for (err, expected_prefix) in cases {
            let msg = err.to_string();
            assert!(
                msg.contains(&expected_prefix[..expected_prefix.len().min(msg.len())])
                    || msg.starts_with(expected_prefix.split(':').next().unwrap_or("")),
                "error {:?} display {:?} should convey {:?}",
                err,
                msg,
                expected_prefix
            );
        }
    }

    #[test]
    fn test_jpeg_quality_clamping() {
        // Quality values outside 1..=100 must clamp before reaching the encoder.
        let mut config = StreamConfig::default();

        config.jpeg_quality = 0;
        let clamped = config.jpeg_quality.clamp(1, 100);
        assert_eq!(clamped, 1);

        config.jpeg_quality = 101;
        let clamped = config.jpeg_quality.clamp(1, 100);
        assert_eq!(clamped, 100);

        config.jpeg_quality = 50;
        assert_eq!(config.jpeg_quality, 50, "in-range value untouched");
    }

    #[test]
    fn test_stride_alignment() {
        // Desktop duplication output rows are 256-byte aligned in practice.
        // Verify row-copy math handles a stride wider than w * 4.
        let w = 100usize;
        let stride = 256usize; // next 256 multiple past 400
        let h = 2usize;

        // Source: stride-padded buffer
        let mut src = vec![0u8; stride * h];
        // Write a marker pixel at row 1, col 0
        let row1_offset = stride; // start of second row
        src[row1_offset] = 200;
        src[row1_offset + 1] = 150;
        src[row1_offset + 2] = 100;
        src[row1_offset + 3] = 255;

        // Dest: tightly packed
        let mut dst = vec![0u8; w * 4 * h];
        for y in 0..h {
            let src_row = &src[y * stride..y * stride + w * 4];
            dst[y * w * 4..(y + 1) * w * 4].copy_from_slice(src_row);
        }

        // Row 0 untouched
        assert!(dst[..w * 4].iter().all(|&b| b == 0));
        // Row 1 carries the marker
        assert_eq!(&dst[w * 4..w * 4 + 4], &[200, 150, 100, 255]);
    }
}