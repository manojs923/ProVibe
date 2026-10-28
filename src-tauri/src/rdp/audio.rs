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
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use windows::core::{Interface, PCWSTR};
#[cfg(windows)]
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK,
};
#[cfg(windows)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
#[cfg(windows)]
use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;

/// 100ns reference-time units per second (WASAPI timestamps use these).
const REFTIMES_PER_SEC: i64 = 10_000_000;
/// Requested buffer capacity passed to IAudioClient::Initialize.
const BUFFER_DURATION_REF: i64 = REFTIMES_PER_SEC; // 1 second
/// Poll interval for the capture loop.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long start() waits for the worker's init handshake.
const INIT_TIMEOUT: Duration = Duration::from_secs(3);
/// Accumulate PCM into segments of this duration before flushing.
const SEGMENT_DURATION_MS: u64 = 1000;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// One chunk of captured PCM audio, ready for the speech-to-text pipeline.
#[derive(Debug, Clone)]
pub struct AudioSegment {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// Host monotonic timestamp when the segment was sealed, ms since epoch.
    pub timestamp_ms: u128,
    /// Monotonically increasing segment counter.
    pub sequence: u64,
    /// 16-bit interleaved PCM, little-endian.
    pub data: Vec<u8>,
}

impl AudioSegment {
    /// Duration of this segment in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        let bytes_per_frame = (self.channels as u64) * 2;
        if bytes_per_frame == 0 {
            return 0;
        }
        (self.data.len() as u64 / bytes_per_frame) * 1000 / self.sample_rate as u64
    }
}

/// Snapshot of the negotiated capture format (from the mix format).
#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub is_float: bool,
}

/// Control messages from the worker thread back to start().
enum InitMsg {
    Ready(AudioFormat),
    Failed(String),
}

/// WASAPI loopback capture tap.
pub struct AudioTap {
    enabled: bool,
    running: Arc<AtomicBool>,
    pending: Vec<AudioSegment>,
    total_bytes_captured: u64,
    worker: Option<JoinHandle<()>>,
    rx: Receiver<AudioSegment>,
    format: Option<AudioFormat>,
    sequence: Arc<AtomicU64>,
}

impl AudioTap {
    /// Construct a new tap. Pass `false` to permanently disable capture —
    /// all methods become no-ops and the tunnel operates visual-only.
    pub fn new(enabled: bool) -> Self {
        let (_tx, rx) = std::sync::mpsc::channel::<AudioSegment>();
        // Keep the sender alive nowhere — the worker builds its own channel
        // in start(); this rx is swapped there. Placeholder receiver so the
        // struct is valid pre-start.
        Self {
            enabled,
            running: Arc::new(AtomicBool::new(false)),
            pending: Vec::new(),
            total_bytes_captured: 0,
            worker: None,
            rx,
            format: None,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Begin capturing redirected audio.
    ///
    /// Returns Ok(()) even when disabled — a disabled tap is a valid state,
    /// not an error. Callers (RDPTunnel::connect) treat audio as optional.
    pub fn start(&mut self) -> Result<(), String> {
        if !self.enabled {
            // Visual-only session — not an error, just no audio.
            return Ok(());
        }

        if self.running.load(Ordering::SeqCst) {
            // Idempotent — already capturing.
            return Ok(());
        }

        let (seg_tx, seg_rx) = std::sync::mpsc::channel::<AudioSegment>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();
        let running = Arc::clone(&self.running);
        let sequence = Arc::clone(&self.sequence);

        running.store(true, Ordering::SeqCst);

        let worker = thread::Builder::new()
            .name("rdp-audio-tap".to_string())
            .spawn(move || {
                capture_loop(running, sequence, seg_tx, init_tx);
            })
            .map_err(|e| format!("audio thread spawn failed: {}", e))?;

        // Block until the worker reports init success/failure.
        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(InitMsg::Ready(fmt)) => {
                self.format = Some(fmt);
                self.rx = seg_rx;
                self.worker = Some(worker);
                self.pending.clear();
                Ok(())
            }
            Ok(InitMsg::Failed(e)) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(e)
            }
            Err(_) => {
                // Timed out — either slow endpoint init or the thread died.
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err("audio init handshake timed out".to_string())
            }
        }
    }

    /// Drain all completed segments since the last call.
    pub fn drain(&mut self) -> Vec<AudioSegment> {
        loop {
            match self.rx.try_recv() {
                Ok(seg) => {
                    self.total_bytes_captured += seg.data.len() as u64;
                    self.pending.push(seg);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        std::mem::take(&mut self.pending)
    }

    /// Stop capture and flush any in-flight segment. Safe to call repeatedly.
    pub fn stop(&mut self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }

        if let Some(worker) = self.worker.take() {
            // Give the loop a moment to notice the flag and flush its tail.
            thread::sleep(POLL_INTERVAL * 3);
            let _ = worker.join();
        }

        // Pull whatever the worker managed to send before exiting.
        self.drain();
    }

    /// Whether the capture loop is currently live.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Negotiated capture format, available after a successful start().
    pub fn format(&self) -> Option<AudioFormat> {
        self.format
    }

    /// Total PCM bytes captured across this tap's lifetime.
    pub fn total_bytes_captured(&self) -> u64 {
        self.total_bytes_captured
    }
}

impl Drop for AudioTap {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Worker — all COM objects live and die on this thread.
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn capture_loop(
    running: Arc<AtomicBool>,
    sequence: Arc<AtomicU64>,
    seg_tx: std::sync::mpsc::Sender<AudioSegment>,
    init_tx: std::sync::mpsc::Sender<InitMsg>,
) {
    // 1. COM init — MTA is fine for audio; if it's already initialized
    //    differently on this thread, fall back to apartment-threaded.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let co_init_mta = hr.is_ok();
    if !co_init_mta {
        let hr2 = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr2.is_err() {
            let _ = init_tx.send(InitMsg::Failed(format!(
                "CoInitializeEx failed: {}",
                hr2
            )));
            return;
        }
    }

        let result = capture_loop_inner(&running, &sequence, &seg_tx, &init_tx);

    // Always uninit COM, regardless of success/failure path.
    unsafe { CoUninitialize() };

    if let Err(e) = result {
        // If init already succeeded, this surfaces as a runtime abort via
        // the running flag flipping; otherwise init_tx will carry the error.
        let _ = init_tx.send(InitMsg::Failed(format!("capture aborted: {}", e)));
    }
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
        let audio_client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| AudioError::DeviceInit(format!("IAudioClient activate: {}", e)))?;

        // 4. Grab the mix format — this is what the engine actually produces.
        let mix_format_ptr = audio_client
            .GetMixFormat()
            .map_err(|e| AudioError::DeviceInit(format!("GetMixFormat: {}", e)))?;

        if mix_format_ptr.is_null() {
            return Err(AudioError::DeviceInit("GetMixFormat returned null".into()));
        }

        let wave_format = &*mix_format_ptr;
        let sample_rate = wave_format.nSamplesPerSec;
        let channels = wave_format.nChannels as u32;
        let bits_per_sample = wave_format.wBitsPerSample as u32;
        let block_align = wave_format.nBlockAlign as u32;

        // 5. Initialize in shared mode with loopback — captures what's being
        //    rendered to the default output (speakers/headphones). This is
        //    exactly what the RDP audio redirection surfaces to the host.
        let hns_buffer_duration: i64 = 10_000_000; // 1 second reference
        audio_client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                hns_buffer_duration,
                0,
                mix_format_ptr,
                None,
            )
            .map_err(|e| AudioError::DeviceInit(format!("IAudioClient::Initialize: {}", e)))?;

        // 6. Capture client — the actual packet pipe.
        let capture_client: IAudioCaptureClient = audio_client
            .GetService()
            .map_err(|e| AudioError::DeviceInit(format!("GetService(IAudioCaptureClient): {}", e)))?;

        // 7. Signal main thread that init succeeded, hand back the format.
        let _ = init_tx.send(InitMsg::Ready {
            sample_rate,
            channels,
            bits_per_sample,
        });

        // 8. Start the stream.
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

        // 9. Pull packets until the running flag flips.
        //    WASAPI pacing: poll at half the buffer period, drain available packets.
        let poll_interval = Duration::from_millis(10);

        while running.load(Ordering::SeqCst) {
            let packet_size = capture_client
                .GetNextPacketSize()
                .map_err(|e| AudioError::CaptureFailure(format!("GetNextPacketSize: {}", e)))?;

            if packet_size == 0 {
                thread::sleep(poll_interval);
                continue;
            }

            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;

            capture_client
                .GetBuffer(
                    &mut data_ptr,
                    &mut num_frames,
                    &mut flags,
                    None,
                    None,
                )
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
                channels: channels as u16,
                bits_per_sample: bits_per_sample as u16,
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
    pub bits_per_sample: u16,
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
        channels: u32,
        bits_per_sample: u32,
    },
    Failed(String),
}

// ---------------------------------------------------------------------------
// AudioTap — the public handle. Owns the worker thread and the segment pipe.
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

pub struct AudioTap {
    enabled: bool,
    running: Arc<AtomicBool>,
    sequence: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
    seg_rx: Option<std::sync::mpsc::Receiver<AudioSegment>>,
    format: Option<AudioFormat>,
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
}

impl AudioTap {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            running: Arc::new(AtomicBool::new(false)),
            sequence: Arc::new(AtomicU64::new(0)),
            worker: None,
            seg_rx: None,
            format: None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn format(&self) -> Option<AudioFormat> {
        self.format
    }

    /// Spawn the WASAPI loopback worker. Blocks briefly until the worker
    /// reports success or failure from inside its COM apartment.
    pub fn start(&mut self) -> Result<(), AudioError> {
        if !self.enabled {
            return Err(AudioError::Disabled);
        }
        if self.running.load(Ordering::SeqCst) {
            return Err(AudioError::AlreadyRunning);
        }

        let (seg_tx, seg_rx) = std::sync::mpsc::channel::<AudioSegment>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let sequence = Arc::clone(&self.sequence);

        let handle = thread::Builder::new()
            .name("rdp-audio-tap".to_string())
            .spawn(move || {
                capture_loop(running, sequence, seg_tx, init_tx);
            })
            .map_err(|e| {
                self.running.store(false, Ordering::SeqCst);
                AudioError::ThreadSpawn(e.to_string())
            })?;

        // Wait up to 2s for the worker to initialize WASAPI.
        let init = init_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| {
                self.running.store(false, Ordering::SeqCst);
                AudioError::InitTimeout
            })?;

        match init {
            InitMsg::Ready {
                sample_rate,
                channels,
                bits_per_sample,
            } => {
                self.format = Some(AudioFormat {
                    sample_rate,
                    channels,
                    bits_per_sample,
                });
                self.worker = Some(handle);
                self.seg_rx = Some(seg_rx);
                Ok(())
            }
            InitMsg::Failed(reason) => {
                self.running.store(false, Ordering::SeqCst);
                let _ = handle.join();
                Err(AudioError::DeviceInit(reason))
            }
        }
    }

    /// Drain all currently queued segments without blocking.
    pub fn drain(&mut self) -> Vec<AudioSegment> {
        let mut out = Vec::new();
        if let Some(rx) = &self.seg_rx {
            while let Ok(seg) = rx.try_recv() {
                out.push(seg);
            }
        }
        out
    }

    /// Block up to `timeout` for the next segment; returns None on timeout.
    pub fn next_segment(&self, timeout: Duration) -> Option<AudioSegment> {
        self.seg_rx.as_ref()?.recv_timeout(timeout).ok()
    }

    pub fn stop(&mut self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        self.seg_rx = None;
    }
}

impl Drop for AudioTap {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Tests — logic-only, no live WASAPI.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_tap_refuses_start() {
        let mut tap = AudioTap::new(false);
        assert!(matches!(tap.start(), Err(AudioError::Disabled)));
    }

    #[test]
    fn drain_empty_returns_empty() {
        let mut tap = AudioTap::new(true);
        assert!(tap.drain().is_empty());
    }

    #[test]
    fn segment_duration_math() {
        let seg = AudioSegment {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 32,
            frame_count: 48_000,
            silent: false,
            timestamp_ms: 0,
            sequence: 0,
            pcm: vec![],
        };
        assert_eq!(seg.duration_ms(), 1000);
    }

    #[test]
    fn mono_f32_handles_wrong_format() {
        let seg = AudioSegment {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 16, // not float32
            frame_count: 10,
            silent: false,
            timestamp_ms: 0,
            sequence: 0,
            pcm: vec![0u8; 40],
        };
        assert!(seg.as_mono_f32().is_empty());
    }

    #[test]
    fn mono_f32_averages_stereo() {
        // 1 frame, 2 channels, float32: left=1.0, right=-1.0 → avg = 0.0
        let left = 1.0f32.to_le_bytes();
        let right = (-1.0f32).to_le_bytes();
        let mut pcm = Vec::new();
        pcm.extend_from_slice(&left);
        pcm.extend_from_slice(&right);

                let seg = AudioSegment {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 32,
            frame_count: 1,
            silent: false,
            timestamp_ms: 0,
            sequence: 0,
            pcm,
        };

        let mono = seg.as_mono_f32();
        assert_eq!(mono.len(), 1);
        assert!((mono[0] - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn mono_f32_single_channel_passthrough() {
        // 3 frames, 1 channel, float32: 0.25, 0.5, -0.75
        let samples = [0.25f32, 0.5, -0.75];
        let mut pcm = Vec::new();
        for s in samples.iter() {
            pcm.extend_from_slice(&s.to_le_bytes());
        }

        let seg = AudioSegment {
            sample_rate: 44_100,
            channels: 1,
            bits_per_sample: 32,
            frame_count: 3,
            silent: false,
            timestamp_ms: 0,
            sequence: 0,
            pcm,
        };

        let mono = seg.as_mono_f32();
        assert_eq!(mono.len(), 3);
        assert!((mono[0] - 0.25).abs() < f32::EPSILON);
        assert!((mono[1] - 0.5).abs() < f32::EPSILON);
        assert!((mono[2] - (-0.75)).abs() < f32::EPSILON);
    }

    #[test]
    fn mono_f32_truncates_partial_frame() {
        // Declare 2 frames but only provide 1.5 frames worth of bytes.
        // Loop should stop at the last complete frame.
        let left = 1.0f32.to_le_bytes();
        let right = (-1.0f32).to_le_bytes();
        let mut pcm = Vec::new();
        pcm.extend_from_slice(&left);
        pcm.extend_from_slice(&right);
        pcm.extend_from_slice(&[0u8, 0u8]); // junk half-sample

        let seg = AudioSegment {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 32,
            frame_count: 2,
            silent: false,
            timestamp_ms: 0,
            sequence: 0,
            pcm,
        };

        let mono = seg.as_mono_f32();
        assert_eq!(mono.len(), 1); // only the first full frame survives
    }

    #[test]
    fn segment_duration_zero_sample_rate() {
        let seg = AudioSegment {
            sample_rate: 0,
            channels: 2,
            bits_per_sample: 32,
            frame_count: 1024,
            silent: false,
            timestamp_ms: 0,
            sequence: 0,
            pcm: vec![],
        };
        assert_eq!(seg.duration_ms(), 0);
    }

    #[test]
    fn audio_tap_double_stop_is_safe() {
        let mut tap = AudioTap::new(true);
        tap.stop();
        tap.stop(); // must not panic, must not deadlock
        assert!(!tap.is_running());
    }

    #[test]
    fn audio_tap_reports_enabled_flag() {
        let tap_on = AudioTap::new(true);
        let tap_off = AudioTap::new(false);
        assert!(tap_on.is_enabled());
        assert!(!tap_off.is_enabled());
    }

    #[test]
    fn audio_tap_format_none_before_start() {
        let tap = AudioTap::new(true);
        assert!(tap.format().is_none());
    }

    #[test]
    fn next_segment_timeout_without_worker() {
        let tap = AudioTap::new(true);
        // No worker spawned → no receiver → returns None.
        assert!(tap.next_segment(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn stopped_tap_not_running() {
        let mut tap = AudioTap::new(true);
        assert!(!tap.is_running());
        tap.stop();
        assert!(!tap.is_running());
    }
}