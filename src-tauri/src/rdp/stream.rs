// src-tauri/src/rdp/stream.rs
//
// Window capture for the RDP client. Two backends:
//   - DXGI Desktop Duplication (fast, GPU, whole-monitor with crop)
//   - GDI PrintWindow (fallback, client-area only)
//
// The capture engine lives entirely on a dedicated worker thread because
// IDXGIOutputDuplication is not Send. The public handle only touches
// Arc-wrapped state.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::rdp::client::RDPClientHandle;

#[cfg(windows)]
use windows::core::{Interface, BOOL};
#[cfg(windows)]
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetClientRect, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible,
};

#[cfg(windows)]
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_9_2, D3D_FEATURE_LEVEL_9_3,
};
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    MonitorFromWindow, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HDC, MONITOR_DEFAULTTONEAREST,
};
#[cfg(windows)]
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible,
};

/// DXGI_ERROR_WAIT_TIMEOUT — no new frame within the acquire window.
const DXGI_WAIT_TIMEOUT: u32 = 0x887A_0027;
/// PW_CLIENTONLY | PW_RENDERFULLCONTENT.
const PW_CLIENT_ONLY_RENDER_FULL: u32 = 0x0000_0003;
/// How long attach() waits for the first frame before giving up.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll granularity inside next_frame().
const FRAME_POLL: Duration = Duration::from_millis(2);
/// Floor for the next_frame() wait, so a tiny acquire_timeout_ms still works.
const FRAME_WAIT_FLOOR_MS: u64 = 500;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("RDP window not found")]
    NoRdpWindow,
    #[error("backend init failed: {0}")]
    BackendInit(String),
    #[error("D3D11 device creation failed")]
    DeviceCreationFailed,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Name used by the tunnel layer.
pub type CaptureConfig = StreamConfig;

// ---------------------------------------------------------------------------
// Pixel helpers
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
// D3D11 device creation
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn init_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), StreamError> {
    let feature_levels: [D3D_FEATURE_LEVEL; 6] = [
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

    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
        .map_err(|e| StreamError::BackendInit(format!("D3D11CreateDevice: {}", e)))?;
    }

    match (device, context) {
        (Some(d), Some(c)) => Ok((d, c)),
        _ => Err(StreamError::DeviceCreationFailed),
    }
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
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
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
    let mut state = EnumState {
        target_pid: process_id,
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut state as *mut _ as isize));
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
// Raw grab result (packed, stride == width * 4)
// ---------------------------------------------------------------------------

#[cfg(windows)]
struct RawFrame {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

#[cfg(windows)]
impl RawFrame {
    fn stride(&self) -> u32 {
        self.width * 4
    }
}

// ---------------------------------------------------------------------------
// DXGI Desktop Duplication engine — built once, reused per frame
// ---------------------------------------------------------------------------

#[cfg(windows)]
struct DdaState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    output_origin: (i32, i32),
    output_size: (u32, u32),
    /// Cached staging texture plus its (width, height, row pitch).
    staging: Option<(ID3D11Texture2D, u32, u32, u32)>,
    acquire_timeout_ms: u32,
}

#[cfg(windows)]
impl DdaState {
    fn new(hwnd: HWND, acquire_timeout_ms: u32) -> Result<Self, StreamError> {
        unsafe {
            let (device, context) = init_d3d_device()?;

            let dxgi_device: IDXGIDevice = device
                .cast()
                .map_err(|e| StreamError::BackendInit(format!("cast IDXGIDevice: {}", e)))?;
            let adapter = dxgi_device
                .GetAdapter()
                .map_err(|e| StreamError::BackendInit(format!("GetAdapter: {}", e)))?;

            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);

            let mut chosen: Option<(IDXGIOutput1, DXGI_OUTPUT_DESC)> = None;
            let mut idx = 0u32;
            loop {
                let output = match adapter.EnumOutputs(idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                if let Ok(desc) = output.GetDesc() {
                    if desc.Monitor == monitor {
                        let out1: IDXGIOutput1 = output
                            .cast()
                            .map_err(|e| StreamError::BackendInit(format!("cast Output1: {}", e)))?;
                        chosen = Some((out1, desc));
                        break;
                    }
                }
                idx += 1;
            }

            let (output1, desc) = match chosen {
                Some(c) => c,
                None => {
                    let output = adapter
                        .EnumOutputs(0)
                        .map_err(|_| StreamError::DuplicationUnsupported)?;
                    let desc = output
                        .GetDesc()
                        .map_err(|_| StreamError::DuplicationUnsupported)?;
                    let out1: IDXGIOutput1 = output
                        .cast()
                        .map_err(|e| StreamError::BackendInit(format!("cast Output1: {}", e)))?;
                    (out1, desc)
                }
            };

            let duplication = output1
                .DuplicateOutput(&device)
                .map_err(|_| StreamError::DuplicationUnsupported)?;

            let coords = desc.DesktopCoordinates;
            let output_origin = (coords.left, coords.top);
            let output_size = (
                (coords.right - coords.left).max(1) as u32,
                (coords.bottom - coords.top).max(1) as u32,
            );

            Ok(Self {
                device,
                context,
                duplication,
                output_origin,
                output_size,
                staging: None,
                acquire_timeout_ms,
            })
        }
    }

    fn grab(&mut self) -> Result<RawFrame, StreamError> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            if let Err(e) = self.duplication.AcquireNextFrame(
                self.acquire_timeout_ms,
                &mut frame_info,
                &mut resource,
            ) {
                return if e.code().0 as u32 == DXGI_WAIT_TIMEOUT {
                    Err(StreamError::FrameTimeout)
                } else {
                    Err(StreamError::CaptureFailed(format!("AcquireNextFrame: {}", e)))
                };
            }

            // Mouse-only update: nothing new to read, and nothing to release
            // beyond the frame we just acquired.
            let resource = match resource {
                Some(r) => r,
                None => {
                    let _ = self.duplication.ReleaseFrame();
                    return Err(StreamError::FrameTimeout);
                }
            };

            let src_tex: ID3D11Texture2D = match resource.cast() {
                Ok(t) => t,
                Err(e) => {
                    let _ = self.duplication.ReleaseFrame();
                    return Err(StreamError::CaptureFailed(format!("cast texture: {}", e)));
                }
            };

            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
            src_tex.GetDesc(&mut src_desc);

            let width = src_desc.Width;
            let height = src_desc.Height;

            // (Re)create the staging texture only when dimensions change.
            let needs_staging = match &self.staging {
                Some((_, w, h, _)) => *w != width || *h != height,
                None => true,
            };

            if needs_staging {
                self.staging = None;

                let mut staging_desc = src_desc;
                staging_desc.Usage = D3D11_USAGE_STAGING;
                staging_desc.BindFlags = 0;
                staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                staging_desc.MiscFlags = 0;

                let mut staging: Option<ID3D11Texture2D> = None;
                if let Err(e) =
                    self.device
                        .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                {
                    let _ = self.duplication.ReleaseFrame();
                    return Err(StreamError::CaptureFailed(format!(
                        "CreateTexture2D staging: {}",
                        e
                    )));
                }
                let staging = match staging {
                    Some(s) => s,
                    None => {
                        let _ = self.duplication.ReleaseFrame();
                        return Err(StreamError::CaptureFailed("no staging texture".into()));
                    }
                };

                self.staging = Some((staging, width, height, width * 4));
            }

            let (staging, _, _, _) = self.staging.as_ref().unwrap();

            self.context.CopyResource(staging, &src_tex);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if let Err(e) = self
                .context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            {
                let _ = self.duplication.ReleaseFrame();
                return Err(StreamError::CaptureFailed(format!("Map: {}", e)));
            }

            let row_pitch = mapped.RowPitch as usize;
            let row_bytes = (width as usize) * 4;
            let mut data = vec![0u8; row_bytes * height as usize];

            let src = mapped.pData as *const u8;
            for row in 0..height as usize {
                let src_row = src.add(row * row_pitch);
                let dst_off = row * row_bytes;
                std::ptr::copy_nonoverlapping(
                    src_row,
                    data.as_mut_ptr().add(dst_off),
                    row_bytes,
                );
            }

            self.context.Unmap(staging, 0);
            let _ = self.duplication.ReleaseFrame();

            Ok(RawFrame {
                data,
                width,
                height,
            })
        }
    }

    fn output_geometry(&self) -> ((i32, i32), (u32, u32)) {
        (self.output_origin, self.output_size)
    }
}

// ---------------------------------------------------------------------------
// GDI PrintWindow engine
// ---------------------------------------------------------------------------

#[cfg(windows)]
struct GdiState {
    hwnd: HWND,
    mem_dc: HDC,
    bmp: HBITMAP,
    bi: BITMAPINFO,
    buf: Vec<u8>,
    width: u32,
    height: u32,
}

#[cfg(windows)]
impl GdiState {
    fn new(hwnd: HWND) -> Result<Self, StreamError> {
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect)
                .map_err(|e| StreamError::GdiFailure(format!("GetClientRect: {}", e)))?;
            let width = (rect.right - rect.left).max(1) as u32;
            let height = (rect.bottom - rect.top).max(1) as u32;

            let window_dc = GetDC(Some(hwnd));
            if window_dc.0.is_null() {
                return Err(StreamError::GdiFailure("GetDC returned null".into()));
            }

            let mem_dc = CreateCompatibleDC(Some(window_dc));
            if mem_dc.0.is_null() {
                ReleaseDC(Some(hwnd), window_dc);
                return Err(StreamError::GdiFailure("CreateCompatibleDC failed".into()));
            }

            let bmp = CreateCompatibleBitmap(window_dc, width as i32, height as i32);
            ReleaseDC(Some(hwnd), window_dc);

            if bmp.0.is_null() {
                let _ = DeleteDC(mem_dc);
                return Err(StreamError::GdiFailure("CreateCompatibleBitmap failed".into()));
            }

            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = width as i32;
            bi.bmiHeader.biHeight = -(height as i32); // top-down
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;

            let buf = vec![0u8; (width as usize) * (height as usize) * 4];

            Ok(Self {
                hwnd,
                mem_dc,
                bmp,
                bi,
                buf,
                width,
                height,
            })
        }
    }

    /// Reallocate the bitmap if the client area changed size.
    fn ensure_size(&mut self, width: u32, height: u32) -> Result<(), StreamError> {
        if self.width == width && self.height == height {
            return Ok(());
        }

        unsafe {
            let window_dc = GetDC(Some(self.hwnd));
            if window_dc.0.is_null() {
                return Err(StreamError::GdiFailure("GetDC returned null".into()));
            }
            let new_bmp = CreateCompatibleBitmap(window_dc, width as i32, height as i32);
            ReleaseDC(Some(self.hwnd), window_dc);

            if new_bmp.0.is_null() {
                return Err(StreamError::GdiFailure("CreateCompatibleBitmap failed".into()));
            }

            let _ = DeleteObject(self.bmp.into());
            self.bmp = new_bmp;
            self.width = width;
            self.height = height;
            self.bi.bmiHeader.biWidth = width as i32;
            self.bi.bmiHeader.biHeight = -(height as i32);
            self.buf = vec![0u8; (width as usize) * (height as usize) * 4];
        }

        Ok(())
    }

    fn grab(&mut self) -> Result<RawFrame, StreamError> {
        unsafe {
            if !IsWindowVisible(self.hwnd).as_bool() {
                return Err(StreamError::WindowGone);
            }

            let mut rect = RECT::default();
            GetClientRect(self.hwnd, &mut rect)
                .map_err(|e| StreamError::GdiFailure(format!("GetClientRect: {}", e)))?;

            let width = (rect.right - rect.left).max(1) as u32;
            let height = (rect.bottom - rect.top).max(1) as u32;
            self.ensure_size(width, height)?;

            let old = SelectObject(self.mem_dc, self.bmp.into());

            // Client-only so the DC size matches the capture size, plus
            // full-content so DWM-composited/occluded pixels still land.
            let ok = PrintWindow(
                self.hwnd,
                self.mem_dc,
                PRINT_WINDOW_FLAGS(PW_CLIENT_ONLY_RENDER_FULL),
            );

            // A bitmap must not be selected into the DC during GetDIBits.
            SelectObject(self.mem_dc, old);

            if !ok.as_bool() {
                return Err(StreamError::GdiFailure("PrintWindow returned false".into()));
            }

            let scanned = GetDIBits(
                self.mem_dc,
                self.bmp,
                0,
                height,
                Some(self.buf.as_mut_ptr() as *mut _),
                &mut self.bi,
                DIB_RGB_COLORS,
            );
            if scanned == 0 {
                return Err(StreamError::GdiFailure("GetDIBits returned 0".into()));
            }

            Ok(RawFrame {
                data: self.buf.clone(),
                width,
                height,
            })
        }
    }
}

#[cfg(windows)]
impl Drop for GdiState {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.mem_dc);
            let _ = DeleteObject(self.bmp.into());
        }
    }
}

// ---------------------------------------------------------------------------
// Engine dispatch
// ---------------------------------------------------------------------------

#[cfg(windows)]
enum Engine {
    Dda(Box<DdaState>),
    Gdi(Box<GdiState>),
}

#[cfg(windows)]
impl Engine {
    /// Build the requested engine, degrading to GDI if duplication is
    /// unavailable on this adapter.
    fn new(backend: CaptureBackend, hwnd: HWND, acquire_timeout_ms: u32) -> Result<Self, StreamError> {
        match backend {
            CaptureBackend::DesktopDuplication => match DdaState::new(hwnd, acquire_timeout_ms) {
                Ok(s) => Ok(Engine::Dda(Box::new(s))),
                Err(dda_err) => match GdiState::new(hwnd) {
                    Ok(s) => {
                        tracing::warn!(
                            error = %dda_err,
                            "desktop duplication unavailable; falling back to GDI"
                        );
                        Ok(Engine::Gdi(Box::new(s)))
                    }
                    Err(gdi_err) => Err(StreamError::BackendInit(format!(
                        "duplication: {}; gdi: {}",
                        dda_err, gdi_err
                    ))),
                },
            },
            CaptureBackend::Gdi => Ok(Engine::Gdi(Box::new(GdiState::new(hwnd)?))),
        }
    }

    fn is_dda(&self) -> bool {
        matches!(self, Engine::Dda(_))
    }

    fn grab(&mut self) -> Result<RawFrame, StreamError> {
        match self {
            Engine::Dda(s) => s.grab(),
            Engine::Gdi(s) => s.grab(),
        }
    }

    /// Drop the DDA path after a hard failure and continue on GDI.
    fn degrade(&mut self, hwnd: HWND) -> Result<(), StreamError> {
        *self = Engine::Gdi(Box::new(GdiState::new(hwnd)?));
        Ok(())
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
    last_seen_seq: u64,
}

/// Name used by the tunnel layer.
pub type StreamCapture = RdpStream;

// HWND is a plain handle; all access is behind our own synchronization.
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
            last_seen_seq: 0,
        }
    }

    /// Tunnel-facing entry point: attach by client handle and wait for the
    /// first frame before returning.
    #[cfg(windows)]
    pub async fn attach(
        handle: &RDPClientHandle,
        config: StreamConfig,
    ) -> Result<Self, StreamError> {
        let hwnd = handle
            .get_window_handle()
            .map_err(|_| StreamError::NoRdpWindow)?;

        let mut stream = Self::new(config);
        stream.rdp_client_pid = Some(handle.pid());
        stream.start_hwnd_waited(hwnd)?;
        Ok(stream)
    }

    #[cfg(not(windows))]
    pub async fn attach(
        _handle: &RDPClientHandle,
        _config: StreamConfig,
    ) -> Result<Self, StreamError> {
        Err(StreamError::BackendInit("non-windows platform".into()))
    }

    /// Wait for a frame newer than the last one handed out.
    pub async fn next_frame(&mut self) -> Result<Frame, StreamError> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(StreamError::NotStarted);
        }

        let wait_ms = (self.config.acquire_timeout_ms as u64 * 4).max(FRAME_WAIT_FLOOR_MS);
        let deadline = Instant::now() + Duration::from_millis(wait_ms);

        loop {
            if !self.running.load(Ordering::SeqCst) {
                return Err(StreamError::WindowGone);
            }

            let seq = self.sequence.load(Ordering::SeqCst);
            if seq != self.last_seen_seq {
                if let Some(frame) = self.take_frame() {
                    self.last_seen_seq = seq;
                    return Ok(frame);
                }
            }

            if Instant::now() >= deadline {
                return Err(match self.last_error() {
                    Some(e) => StreamError::CaptureFailed(e),
                    None => StreamError::FrameTimeout,
                });
            }

            thread::sleep(FRAME_POLL);
        }
    }

    /// Stop the worker and release the engine.
    pub async fn detach(mut self) -> Result<(), StreamError> {
        self.stop();
        Ok(())
    }

    /// Attach to a running RdpClient, locate its window, and start capturing.
    #[cfg(windows)]
    pub fn attach_to_client_window(&mut self, client: &crate::rdp::client::RdpClient) -> Result<(), StreamError> {
        let pid = client.process_id().ok_or(StreamError::NoRdpWindow)?;
        self.rdp_client_pid = Some(pid);
        let hwnd = find_rdp_window(pid)?;
        self.start_hwnd(hwnd)
    }

    #[cfg(not(windows))]
    pub fn attach_to_client_window(
        &mut self,
        _client: &crate::rdp::client::RdpClient,
    ) -> Result<(), StreamError> {
        Err(StreamError::BackendInit("non-windows platform".into()))
    }

    #[cfg(windows)]
    pub fn start_hwnd(&mut self, hwnd: HWND) -> Result<(), StreamError> {
        self.spawn_worker(hwnd, false)
    }

    /// Start and block until the first frame exists (or fail).
    #[cfg(windows)]
    fn start_hwnd_waited(&mut self, hwnd: HWND) -> Result<(), StreamError> {
        self.spawn_worker(hwnd, true)
    }

    #[cfg(not(windows))]
    pub fn start_hwnd(&mut self, _hwnd: usize) -> Result<(), StreamError> {
        Err(StreamError::BackendInit("non-windows platform".into()))
    }

    #[cfg(windows)]
    fn spawn_worker(&mut self, hwnd: HWND, wait_for_first: bool) -> Result<(), StreamError> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.hwnd = Some(hwnd);
        self.latest_frame.lock().unwrap().take();
        *self.last_error.lock().unwrap() = None;
        self.running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.running);
        let sequence = Arc::clone(&self.sequence);
        let last_error = Arc::clone(&self.last_error);
        let latest_frame = Arc::clone(&self.latest_frame);
        let config = self.config.clone();
        let hwnd_raw = hwnd.0 as isize;

        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), StreamError>>();

        let worker = thread::Builder::new()
            .name("rdp-capture".to_string())
            .spawn(move || {
                let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
                let frame_interval = config.frame_interval();
                let mut next_frame_at = Instant::now();

                // Engine construction happens here because the duplication
                // object is thread-affine.
                let mut engine = match Engine::new(config.backend, hwnd, config.acquire_timeout_ms) {
                    Ok(e) => {
                        let _ = ready_tx.send(Ok(()));
                        e
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(StreamError::BackendInit(e.to_string())));
                        running.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let mut first_frame_sent = false;

                while running.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    if now < next_frame_at {
                        thread::sleep(next_frame_at - now);
                    }
                    next_frame_at = Instant::now() + frame_interval;

                    let mut raw = match engine.grab() {
                        Ok(f) => f,
                        Err(StreamError::FrameTimeout) => continue,
                        Err(StreamError::WindowGone) => {
                            *last_error.lock().unwrap() = Some("window gone".into());
                            break;
                        }
                        Err(e) => {
                            *last_error.lock().unwrap() = Some(e.to_string());
                            if engine.is_dda() {
                                // Duplication can die on mode changes; rebuild
                                // once, then degrade permanently.
                                if let Err(e2) = engine.degrade(hwnd) {
                                    *last_error.lock().unwrap() = Some(e2.to_string());
                                    thread::sleep(Duration::from_millis(100));
                                    continue;
                                }
                            } else {
                                thread::sleep(Duration::from_millis(100));
                            }
                            continue;
                        }
                    };

                    // Crop to the client area when using duplication; the
                    // GDI path already captures only the client area.
                    if config.crop_to_window {
                        if let Engine::Dda(dda) = &engine {
                            let (origin, size) = dda.output_geometry();
                            let crop =
                                compute_crop_rect(window_screen_rect(hwnd), origin, size);
                            if crop.2 > 0 && crop.3 > 0 {
                                let cropped = crop_bgra(&raw.data, raw.width, raw.stride(), crop);
                                if !cropped.is_empty() {
                                    raw.data = cropped;
                                    raw.width = crop.2;
                                    raw.height = crop.3;
                                }
                            }
                        }
                    }

                    if let Some((dw, dh)) = config.downscale {
                        let scaled = downscale_bgra(&raw.data, raw.width, raw.height, dw, dh);
                        if !scaled.is_empty() {
                            raw.data = scaled;
                            raw.width = dw;
                            raw.height = dh;
                        }
                    }

                    let frame = Frame {
                        width: raw.width,
                        height: raw.height,
                        stride: raw.stride(),
                        data: raw.data,
                        timestamp_ms: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0),
                        sequence: sequence.fetch_add(1, Ordering::SeqCst),
                    };

                    *latest_frame.lock().unwrap() = Some(frame);
                    first_frame_sent = true;
                }

                let _ = first_frame_sent;
            })
            .map_err(|e| StreamError::ThreadSpawn(e.to_string()))?;

        self.worker = Some(worker);

        if wait_for_first {
            // Engine construction reports back before the first frame; then
            // wait for an actual frame so callers never see an empty slot.
            match ready_rx.recv_timeout(ATTACH_TIMEOUT) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    self.stop();
                    return Err(e);
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.stop();
                    return Err(StreamError::BackendInit(
                        "capture engine did not initialize in time".into(),
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.stop();
                    return Err(StreamError::ChannelClosed);
                }
            }

            let deadline = Instant::now() + ATTACH_TIMEOUT;
            loop {
                if self.latest_frame.lock().unwrap().is_some() {
                    break;
                }
                if !self.running.load(Ordering::SeqCst) {
                    let err = self.last_error();
                    self.stop();
                    return Err(match err {
                        Some(e) => StreamError::CaptureFailed(e),
                        None => StreamError::WindowGone,
                    });
                }
                if Instant::now() >= deadline {
                    self.stop();
                    return Err(StreamError::FrameTimeout);
                }
                thread::sleep(FRAME_POLL);
            }
        }

        Ok(())
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
}

impl Drop for RdpStream {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// One-shot capture of the RDP window as base64 JPEG
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn capture_rdp_window_once(pid: u32) -> Result<String, StreamError> {
    let hwnd = find_rdp_window(pid)?;
    let mut state = GdiState::new(hwnd)?;
    let raw = state.grab()?;
    drop(state);

    let (data, width, height) = (raw.data, raw.width, raw.height);
    let stride = (width as usize) * 4;

    // BGRA → RGB for JPEG encoding.
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let i = row_start + x * 4;
            if i + 3 <= data.len() {
                rgb.push(data[i + 2]);
                rgb.push(data[i + 1]);
                rgb.push(data[i]);
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

// ---------------------------------------------------------------------------
// One-shot capture of the RDP window as base64 JPEG
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn encode_raw_to_base64_jpeg(raw: RawFrame) -> Result<String, StreamError> {
    let (data, width, height) = (raw.data, raw.width, raw.height);
    let stride = (width as usize) * 4;

    // BGRA -> RGB for JPEG encoding.
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let i = row_start + x * 4;
            if i + 3 <= data.len() {
                rgb.push(data[i + 2]);
                rgb.push(data[i + 1]);
                rgb.push(data[i]);
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

/// One-shot client-area capture of an already-resolved window.
#[cfg(windows)]
pub fn capture_hwnd_once(hwnd: HWND) -> Result<String, StreamError> {
    let mut state = GdiState::new(hwnd)?;
    let raw = state.grab()?;
    drop(state);
    encode_raw_to_base64_jpeg(raw)
}

/// One-shot capture of the client area of the first RDP window owned by `pid`.
#[cfg(windows)]
pub fn capture_rdp_window_once(pid: u32) -> Result<String, StreamError> {
    capture_hwnd_once(find_rdp_window(pid)?)
}

#[cfg(not(windows))]
pub fn capture_rdp_window_once(_pid: u32) -> Result<String, StreamError> {
    Err(StreamError::BackendInit("non-windows platform".into()))
}

// ---------------------------------------------------------------------------
// Console window lookup by title hint
//
// Callers usually have a VM name or an IP, not a pid, so the hint is matched
// against window titles and the console class names. Anything that looks like a
// VM console scores; the caller's hint scores highest. Ties break on client
// area, so a maximised console wins over a stray dialog.
// ---------------------------------------------------------------------------

#[cfg(windows)]
const CONSOLE_TITLE_HINTS: [&str; 2] = ["virtual machine connection", "remote desktop connection"];

#[cfg(windows)]
const CONSOLE_CLASSES: [&str; 3] = ["vmconnectui", "tscshellcontainerclass", "mstsc"];

#[cfg(windows)]
struct HintState {
    hint: String,
    best: Option<(u32, i64, isize)>,
}

#[cfg(windows)]
unsafe extern "system" fn hint_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut HintState);

    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w < 400 || h < 300 {
        return BOOL(1);
    }

    let mut title_buf = [0u16; 256];
    let title_len = GetWindowTextW(hwnd, &mut title_buf);
    let title = if title_len > 0 {
        String::from_utf16_lossy(&title_buf[..title_len as usize]).to_lowercase()
    } else {
        String::new()
    };

    let mut class_buf = [0u16; 128];
    let class_len = GetClassNameW(hwnd, &mut class_buf);
    let class = if class_len > 0 {
        String::from_utf16_lossy(&class_buf[..class_len as usize]).to_lowercase()
    } else {
        String::new()
    };

    let mut score = 0u32;
    if !state.hint.is_empty() && title.contains(&state.hint) {
        score += 3;
    }
    if CONSOLE_TITLE_HINTS.iter().any(|needle| title.contains(needle)) {
        score += 2;
    }
    if CONSOLE_CLASSES.iter().any(|needle| class.contains(needle)) {
        score += 1;
    }
    if score == 0 {
        return BOOL(1);
    }

    let area = (w as i64) * (h as i64);
    let hwnd_raw = hwnd.0 as isize;

    let better = match state.best {
        None => true,
        Some((best_score, best_area, _)) => {
            score > best_score || (score == best_score && area > best_area)
        }
    };

    if better {
        state.best = Some((score, area, hwnd_raw));
    }

    BOOL(1)
}

/// Find the best-matching visible VM console window for `hint`.
#[cfg(windows)]
pub fn find_console_window(hint: &str) -> Result<HWND, StreamError> {
    let mut state = HintState {
        hint: hint.trim().to_lowercase(),
        best: None,
    };

    unsafe {
        let _ = EnumWindows(Some(hint_proc), LPARAM(&mut state as *mut state as isize));
    }

    state
        .best
        .map(|(_, _, h)| HWND(h as *mut core::ffi::c_void))
        .ok_or(StreamError::NoRdpWindow)
}

/// Resolve `hint` to a console window and capture it as base64 JPEG.
#[cfg(windows)]
pub fn capture_window_by_hint(hint: &str) -> Result<String, StreamError> {
    capture_hwnd_once(find_console_window(hint)?)
}

#[cfg(not(windows))]
pub fn capture_window_by_hint(_hint: &str) -> Result<String, StreamError> {
    Err(StreamError::BackendInit("non-windows platform".into()))
}