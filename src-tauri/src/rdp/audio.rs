//! src-tauri/src/rdp/audio.rs
//!
//! WASAPI loopback tap for RDP audio redirection.
//!
//! mstsc renders guest audio into the host's default render endpoint.
//! This module opens that endpoint in loopback mode and captures the PCM
//! stream — that's the audio half of the optical air-gap. We never touch
//! the VM; we only record what the host speakers would have played.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::rdp::client::RDPClientHandle;

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEXTENSIBLE,
};
#[cfg(windows)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
};

/// 100ns reference-time units per second (WASAPI timestamps use these).
const REFTIMES_PER_SEC: i64 = 10_000_000;
/// Requested buffer capacity passed to IAudioClient::Initialize.
#[cfg(windows)]
const BUFFER_DURATION_REF: i64 = REFTIMES_PER_SEC; // 1 second
/// Poll interval for the capture loop.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long start() waits for the worker's init handshake.
const INIT_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(windows)]
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
#[cfg(windows)]
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Snapshot of the negotiated capture format (from the mix format).
#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub is_float: bool,
}

/// WASAPI loopback capture tap.
pub struct AudioTap {
    enabled: bool,
    running: Arc<AtomicBool>,
    pending: Vec<AudioSegment>,
    total_bytes_captured: u64,
    worker: Option<JoinHandle<()>>,
    /// Mutex so the tap is Sync and the owning Tunnel can live in Tauri state.
    seg_rx: Mutex<Receiver<AudioSegment>>,
    format: Option<AudioFormat>,
    sequence: Arc<AtomicU64>,
    /// PID of the mstsc client this tap is attached to.
    client_pid: Option<u32>,
}

impl AudioTap {
    /// Construct a new tap. Pass `false` to permanently disable capture —
    /// all methods become no-ops and the tunnel operates visual-only.
    pub fn new(enabled: bool) -> Self {
        // Placeholder receiver so the struct is valid pre-start; the real
        // channel is created in start() and swapped in.
        let (_tx, rx) = std::sync::mpsc::channel::<AudioSegment>();
        Self {
            enabled,
            running: Arc::new(AtomicBool::new(false)),
            pending: Vec::new(),
            total_bytes_captured: 0,
            worker: None,
            seg_rx: Mutex::new(rx),
            format: None,
            sequence: Arc::new(AtomicU64::new(0)),
            client_pid: None,
        }
    }

    /// Begin capturing redirected audio for the given client.
    ///
    /// Returns Ok(()) even when disabled — a disabled tap is a valid state,
    /// not an error. Callers (Tunnel::connect) treat audio as optional.
    pub fn start(&mut self, handle: &RDPClientHandle) -> Result<(), AudioError> {
        if !self.enabled {
            return Ok(());
        }

        if self.running.load(Ordering::SeqCst) {
            // Idempotent — already capturing.
            return Ok(());
        }

        let pid = handle.pid();
        if pid == 0 {
            return Err(AudioError::DeviceInit("client pid is 0".into()));
        }

        let (seg_tx, seg_rx) = std::sync::mpsc::channel::<AudioSegment>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();

        self.running.store(true, Ordering::SeqCst);

        // Clone the Arcs before moving into the thread so `running` is still
        // usable on the failure/timeout paths below.
        let running_thread = Arc::clone(&self.running);
        let sequence_thread = Arc::clone(&self.sequence);

        let worker = match thread::Builder::new()
            .name("rdp-audio-tap".to_string())
            .spawn(move || {
                capture_loop(running_thread, sequence_thread, seg_tx, init_tx);
            }) {
            Ok(w) => w,
            Err(e) => {
                self.running.store(false, Ordering::SeqCst);
                return Err(AudioError::ThreadSpawn(e.to_string()));
            }
        };

        // Block until the worker reports init success/failure. Success is
        // only signalled after the stream is actually running.
        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(InitMsg::Ready {
                sample_rate,
                channels,
                bits_per_sample,
                is_float,
            }) => {
                self.format = Some(AudioFormat {
                    sample_rate,
                    channels,
                    bits_per_sample: bits_per_sample as u16,
                    is_float,
                });
                self.client_pid = Some(pid);
                *self.seg_rx.lock().unwrap() = seg_rx;
                self.worker = Some(worker);
                self.pending.clear();
                Ok(())
            }
            Ok(InitMsg::Failed(e)) => {
                self.running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(AudioError::DeviceInit(e))
            }
            Err(_) => {
                // Timed out — either slow endpoint init or the thread died.
                self.running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(AudioError::InitTimeout)
            }
        }
    }

    /// Pull everything currently queued from the worker into `pending`.
    fn collect(&mut self) {
        let rx = self.seg_rx.get_mut().unwrap();
        loop {
            match rx.try_recv() {
                Ok(seg) => {
                    self.total_bytes_captured += seg.pcm.len() as u64;
                    self.pending.push(seg);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    /// Drain all completed segments since the last call.
    pub fn drain(&mut self) -> Vec<AudioSegment> {
        self.collect();
        std::mem::take(&mut self.pending)
    }

    /// Stop capture and flush any in-flight segment. Safe to call repeatedly.
    /// Segments captured up to shutdown remain available via `drain()`.
    pub fn stop(&mut self) -> Result<(), AudioError> {
        if !self.running.swap(false, Ordering::SeqCst) {
            return Ok(());
        }

        if let Some(worker) = self.worker.take() {
            // The loop notices the flag within one poll interval.
            worker
                .join()
                .map_err(|_| AudioError::CaptureFailure("audio worker panicked".into()))?;
        }

        // Keep the tail in `pending` so the next drain() returns it.
        self.collect();
        Ok(())
    }

    /// Whether the capture loop is currently live.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Negotiated capture format, available after a successful start().
    pub fn format(&self) -> Option<AudioFormat> {
        self.format
    }

    /// True when the endpoint produces 32-bit float samples.
    pub fn is_float(&self) -> bool {
        self.format.map(|f| f.is_float).unwrap_or(false)
    }

    /// PID this tap was started against.
    pub fn client_pid(&self) -> Option<u32> {
        self.client_pid
    }

    /// Total PCM bytes captured across this tap's lifetime.
    pub fn total_bytes_captured(&self) -> u64 {
        self.total_bytes_captured
    }
}

impl Drop for AudioTap {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ---------------------------------------------------------------------------
// Worker — all COM objects live and die on this thread.
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn get_audio_client(device: &IMMDevice) -> Result<IAudioClient, AudioError> {
    unsafe {
        device
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(|e| AudioError::DeviceInit(format!("IAudioClient activate: {:?}", e)))
    }
}

#[cfg(windows)]
fn capture_loop(
    running: Arc<AtomicBool>,
    sequence: Arc<AtomicU64>,
    seg_tx: std::sync::mpsc::Sender<AudioSegment>,
    init_tx: std::sync::mpsc::Sender<InitMsg>,
) {
    // 1. COM init — MTA preferred; fall back to STA if the thread is already
    //    initialized differently. Only uninitialize if init actually succeeded.
    let mut com_initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if !com_initialized {
        let hr2 = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr2.is_err() {
            let _ = init_tx.send(InitMsg::Failed(format!("CoInitializeEx failed: {}", hr2)));
            return;
        }
        com_initialized = true;
    }

    // All COM objects are created and dropped inside this call, before
    // CoUninitialize runs.
    let result = capture_loop_inner(&running, &sequence, &seg_tx, &init_tx);

    if com_initialized {
        unsafe { CoUninitialize() };
    }

    if let Err(e) = result {
        // If init already succeeded the receiver is no longer listening and
        // this send is a harmless no-op; otherwise it carries the init error.
        let _ = init_tx.send(InitMsg::Failed(format!("capture aborted: {}", e)));
    }

    // Make sure is_running() reflects reality if the worker exits on its own.
    running.store(false, Ordering::SeqCst);
}

#[cfg(windows)]
fn capture_loop_inner(
    running: &Arc<AtomicBool>,
    sequence: &Arc<AtomicU64>,
    seg_tx: &std::sync::mpsc::Sender<AudioSegment>,
    init_tx: &std::sync::mpsc::Sender<InitMsg>,
) -> Result<(), AudioError> {
    unsafe {
        // 2. Enumerate the default render endpoint (we loopback-capture it).
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| AudioError::DeviceInit(format!("CoCreateInstance: {}", e)))?;

        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| AudioError::DeviceInit(format!("GetDefaultAudioEndpoint: {}", e)))?;

        // 3. Activate the IAudioClient on that endpoint.
        let audio_client = get_audio_client(&device)?;

        // 4. Grab the mix format — this is what the engine actually produces.
        let mix_format_ptr = audio_client
            .GetMixFormat()
            .map_err(|e| AudioError::DeviceInit(format!("GetMixFormat: {}", e)))?;

        if mix_format_ptr.is_null() {
            return Err(AudioError::DeviceInit("GetMixFormat returned null".into()));
        }

        // WAVEFORMATEX is packed: read fields by value, never take references.
        let wave_format = &*mix_format_ptr;
        let sample_rate = wave_format.nSamplesPerSec;
        let channels = wave_format.nChannels;
        let bits_per_sample = wave_format.wBitsPerSample as u8;
        let block_align = wave_format.nBlockAlign as u32;
        let format_tag = wave_format.wFormatTag;

        let is_float = match format_tag {
            WAVE_FORMAT_IEEE_FLOAT => true,
            WAVE_FORMAT_EXTENSIBLE => {
                // The first GUID field of the SubFormat mirrors the format tag.
                let ext = mix_format_ptr as *const WAVEFORMATEXTENSIBLE;
                let sub = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
                sub.data1 == WAVE_FORMAT_IEEE_FLOAT as u32
            }
            _ => false,
        };

        // 5. Initialize in shared mode with loopback — captures what's being
        //    rendered to the default output (speakers/headphones).
        let init_result = audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            BUFFER_DURATION_REF,
            0,
            mix_format_ptr,
            None,
        );

        // The mix format is CoTaskMem-allocated; we've copied what we need.
        CoTaskMemFree(Some(mix_format_ptr as *const c_void));

        init_result
            .map_err(|e| AudioError::DeviceInit(format!("IAudioClient::Initialize: {}", e)))?;

        // 6. Capture client — the actual packet pipe.
        let capture_client: IAudioCaptureClient = audio_client
            .GetService()
            .map_err(|e| AudioError::DeviceInit(format!("GetService(IAudioCaptureClient): {}", e)))?;

        // 7. Start the stream *before* signalling readiness, so a Start()
        //    failure surfaces as a start() error instead of a silent
        //    half-initialized tap.
        audio_client
            .Start()
            .map_err(|e| AudioError::DeviceInit(format!("IAudioClient::Start: {}", e)))?;

        // Guard so we always Stop() on any exit path.
        struct StopGuard<'a>(&'a IAudioClient);
        impl<'a> Drop for StopGuard<'a> {
            fn drop(&mut self) {
                unsafe {
                    let _ = self.0.Stop();
                }
            }
        }
        let _stop_guard = StopGuard(&audio_client);

        // 8. Hand back the format now that capture is genuinely live.
        let _ = init_tx.send(InitMsg::Ready {
            sample_rate,
            channels,
            bits_per_sample,
            is_float,
        });

        // 9. Pull packets until the running flag flips.
        while running.load(Ordering::SeqCst) {
            let packet_size = capture_client
                .GetNextPacketSize()
                .map_err(|e| AudioError::CaptureFailure(format!("GetNextPacketSize: {}", e)))?;

            if packet_size == 0 {
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;

            capture_client
                .GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)
                .map_err(|e| AudioError::CaptureFailure(format!("GetBuffer: {}", e)))?;

            if num_frames == 0 {
                let _ = capture_client.ReleaseBuffer(0);
                continue;
            }

            let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let byte_count = (num_frames * block_align) as usize;

            let mut pcm = vec![0u8; byte_count];
            if !silent && !data_ptr.is_null() {
                std::ptr::copy_nonoverlapping(data_ptr, pcm.as_mut_ptr(), byte_count);
            }
            // On silent flag we keep the zero-filled buffer — the STT pipeline
            // benefits from knowing there was a gap rather than losing timing.

            let _ = capture_client.ReleaseBuffer(num_frames);

            let seg = AudioSegment {
                sample_rate,
                channels,
                bits_per_sample,
                frame_count: num_frames,
                silent,
                timestamp_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                sequence: sequence.fetch_add(1, Ordering::SeqCst),
                pcm,
            };

            if seg_tx.send(seg).is_err() {
                // Receiver dropped — the AudioTap owner is gone, bail cleanly.
                break;
            }
        }

        Ok(())
    }
}

#[cfg(not(windows))]
fn capture_loop(
    _running: Arc<AtomicBool>,
    _sequence: Arc<AtomicU64>,
    _seg_tx: std::sync::mpsc::Sender<AudioSegment>,
    init_tx: std::sync::mpsc::Sender<InitMsg>,
) {
    let _ = init_tx.send(InitMsg::Failed(
        "WASAPI loopback capture only supported on Windows".into(),
    ));
}

// ---------------------------------------------------------------------------
// AudioSegment — one WASAPI packet, decoded enough for the STT pipeline.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AudioSegment {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u8,
    pub frame_count: u32,
    pub silent: bool,
    pub timestamp_ms: u128,
    pub sequence: u64,
    /// Raw PCM bytes in the mix format's native layout (usually float32 interleaved).
    pub pcm: Vec<u8>,
}

impl AudioSegment {
    /// Duration of this segment in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            return 0;
        }
        (self.frame_count as u64 * 1000) / self.sample_rate as u64
    }

    /// Convert interleaved float32 PCM to mono f32 samples (averaged channels).
    /// Returns empty vec if the format isn't float32.
    pub fn as_mono_f32(&self) -> Vec<f32> {
        if self.bits_per_sample != 32 || self.channels == 0 {
            return Vec::new();
        }
        let bytes_per_sample = 4;
        let bytes_per_frame = bytes_per_sample * self.channels as usize;
        if self.pcm.len() < bytes_per_frame {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(self.frame_count as usize);
        for frame_idx in 0..self.frame_count as usize {
            let frame_start = frame_idx * bytes_per_frame;
            if frame_start + bytes_per_frame > self.pcm.len() {
                break;
            }
            let mut acc = 0f32;
            for ch in 0..self.channels as usize {
                let off = frame_start + ch * bytes_per_sample;
                let s = f32::from_le_bytes([
                    self.pcm[off],
                    self.pcm[off + 1],
                    self.pcm[off + 2],
                    self.pcm[off + 3],
                ]);
                acc += s;
            }
            out.push(acc / self.channels as f32);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// InitMsg — handshake between worker and owner.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum InitMsg {
    Ready {
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u8,
        is_float: bool,
    },
    Failed(String),
}

// ---------------------------------------------------------------------------
// AudioError — error type for audio capture operations.
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("audio capture disabled by config")]
    Disabled,
    #[error("device init failed: {0}")]
    DeviceInit(String),
    #[error("capture failure: {0}")]
    CaptureFailure(String),
    #[error("worker thread spawn failed: {0}")]
    ThreadSpawn(String),
    #[error("worker init timeout")]
    InitTimeout,
    #[error("already running")]
    AlreadyRunning,
    #[error("not running")]
    NotRunning,
}