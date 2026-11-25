mod hyper_v;
mod patches;
mod rdp;
mod stealth;
mod target_id;
mod util;
mod vm;

pub use target_id::{fetch_all, fetch_profile};

/// Re-exported for the modules that shell out; the definition and its
/// rationale live in [`util`].
pub(crate) use util::hidden_command;

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use rdev::{listen, EventType, Key};
use screenshots::Screen;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongPtrW, SetForegroundWindow, SetWindowDisplayAffinity,
    SetWindowLongPtrW, GWL_EXSTYLE, WDA_EXCLUDEFROMCAPTURE, WS_EX_NOACTIVATE,
};

/// Fallback guest name when `PROTOCOL_VM_NAME` is unset.
const DEFAULT_VM_NAME: &str = "Test-Nartan-Build";

/// Folder under %APPDATA% that holds user-writable state.
const APP_DIR_NAME: &str = "com.protocol.app";

/// Prefix every stored frame carries.
const DATA_URL_PREFIX: &str = "data:image/jpeg;base64,";

/// A key still flagged as held after this long is assumed to have lost its
/// release event (focus steal, hook drop) and is treated as fresh.
const REPEAT_GRACE_MS: u64 = 5_000;

/// Bumped whenever a default binding moves. Drives the migration below so an
/// existing trigger_config.json is rewritten instead of silently fighting the
/// new defaults.
const CONFIG_VERSION: u32 = 3;

/// Set while a provision is in flight so a second ENGAGE cannot start a second
/// New-VHD against the same base disk.
static PROVISIONING: AtomicBool = AtomicBool::new(false);

/// Foreground window captured the moment a text field borrows focus, so it can
/// be handed straight back on release.
static PREV_FOREGROUND: AtomicIsize = AtomicIsize::new(0);

// ---------------------------------------------------------------------------
// Trigger configuration
//
// Lives in %APPDATA%\com.protocol.app\trigger_config.json. A perMachine install
// puts the exe in Program Files, where fs::write fails unelevated, so the file
// cannot sit beside the binary any more. A config left next to the exe by an
// older build is carried over once and then ignored.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerConfig {
    /// Schema stamp. Missing in v1 files, hence the serde default.
    #[serde(default)]
    pub version: u32,
    /// Double-tap to summon the copilot. Default: CapsLock (invisible, net-zero).
    pub summon_double_tap: String,
    /// Double-tap to buffer one frame. Default: "/" (leaves "//").
    pub sniper_double_tap: String,
    /// Double-tap to flush the frame buffer. Default: Return.
    pub flush_double_tap: String,
    /// Double-tap to start/stop audio capture. Default: "." (leaves "..").
    pub audio_double_tap: String,
    /// Double-tap to hide/show the window. Default: "-" (leaves "--").
    pub toggle_double_tap: String,
    /// Double-tap to raise opacity.
    pub opacity_up_double_tap: String,
    /// Double-tap to lower opacity.
    pub opacity_down_double_tap: String,
    /// Max ms between two presses of the same key to count as a double-tap.
    pub double_tap_window_ms: u64,
    /// Cooldown FLOOR for the sniper, in ms. Summon/toggle/opacity ignore this.
    pub cooldown_min_ms: u64,
    /// Cooldown CEILING for the sniper — the gate is jittered inside the range
    /// so repeated captures don't land on a metronome.
    pub cooldown_max_ms: u64,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            summon_double_tap: "CapsLock".to_string(),
            sniper_double_tap: "/".to_string(),
            flush_double_tap: "Return".to_string(),
            audio_double_tap: ".".to_string(),
            toggle_double_tap: "-".to_string(),
            opacity_up_double_tap: "=".to_string(),
            opacity_down_double_tap: "[".to_string(),
            double_tap_window_ms: 400,
            cooldown_min_ms: 800,
            cooldown_max_ms: 1200,
        }
    }
}

impl TriggerConfig {
    /// Brings an older file up to the current binding set. Returns whether
    /// anything was rewritten so the caller can persist it.
    fn migrated(mut self) -> (Self, bool) {
        let mut changed = false;

        // The toggle always owns "-". Anything else on it — the sniper in v1,
        // a stale hand-edit, a collision — is moved off.
        if self.sniper_double_tap == "-" {
            self.sniper_double_tap = "/".to_string();
            changed = true;
        }
        if self.toggle_double_tap != "-" {
            self.toggle_double_tap = "-".to_string();
            changed = true;
        }

        if self.version != CONFIG_VERSION {
            self.version = CONFIG_VERSION;
            changed = true;
        }

        (self, changed)
    }

    /// Rejects tokens that carry a modifier, since the whole point is to keep
    /// the guest-side fingerprint modifier-free.
    fn sanitized(mut self) -> Self {
        let defaults = TriggerConfig::default();

        let fields: [&mut String; 7] = [
            &mut self.summon_double_tap,
            &mut self.sniper_double_tap,
            &mut self.flush_double_tap,
            &mut self.audio_double_tap,
            &mut self.toggle_double_tap,
            &mut self.opacity_up_double_tap,
            &mut self.opacity_down_double_tap,
        ];
        let fallbacks: [&String; 7] = [
            &defaults.summon_double_tap,
            &defaults.sniper_double_tap,
            &defaults.flush_double_tap,
            &defaults.audio_double_tap,
            &defaults.toggle_double_tap,
            &defaults.opacity_up_double_tap,
            &defaults.opacity_down_double_tap,
        ];

        for (slot, fallback) in fields.into_iter().zip(fallbacks) {
            let lowered = slot.to_lowercase();
            if lowered.contains("ctrl")
                || lowered.contains("alt")
                || lowered.contains("shift")
                || lowered.contains("meta")
                || lowered.contains("win")
            {
                eprintln!(
                    "⚠️ trigger '{}' uses a modifier, falling back to '{}'",
                    slot, fallback
                );
                *slot = fallback.clone();
            }
        }

        // Two commands on one key means the first match always wins and the
        // second becomes dead. Nudge the sniper back to its default.
        if self.sniper_double_tap.eq_ignore_ascii_case(&self.toggle_double_tap) {
            eprintln!("⚠️ sniper and toggle share a key, moving the sniper to '/'");
            self.sniper_double_tap = defaults.sniper_double_tap.clone();
        }

        if self.double_tap_window_ms < 120 {
            self.double_tap_window_ms = 400;
        }
        if self.cooldown_max_ms < self.cooldown_min_ms {
            self.cooldown_max_ms = self.cooldown_min_ms;
        }
        self.version = CONFIG_VERSION;
        self
    }
}

/// Writable config directory. %APPDATA% is the only location that works for a
/// perMachine install running unelevated; the exe directory is a last resort
/// for a portable/renamed environment where APPDATA is somehow unset.
fn config_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("APPDATA") {
        Some(appdata) => PathBuf::from(appdata),
        None => {
            let fallback = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))?;
            eprintln!("⚠️ APPDATA unset, keeping config beside the exe");
            return Some(fallback);
        }
    };

    let dir = base.join(APP_DIR_NAME);
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!(
            "⚠️ cannot create {} ({}), keeping config beside the exe",
            dir.display(),
            e
        );
        return std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    }
    Some(dir)
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("trigger_config.json"))
}

/// Where pre-APPDATA builds kept the file.
fn legacy_config_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .map(|d| d.join("trigger_config.json"))
}

/// One-time carry-over of a config left next to the exe.
///
/// Copy, not move: the Program Files original may be read-only and a failed
/// delete must not abort startup. The legacy file is left in place as a backup
/// and never read again once the new one exists. Only valid JSON is carried
/// over — a malformed legacy file is skipped so it cannot re-break the app on
/// every launch.
fn migrate_legacy_config(target: &Path) {
    if target.exists() {
        return;
    }
    let Some(legacy) = legacy_config_path() else {
        return;
    };
    if !legacy.exists() || legacy == target {
        return;
    }
    let Ok(raw) = fs::read_to_string(&legacy) else {
        return;
    };
    if serde_json::from_str::<TriggerConfig>(&raw).is_err() {
        eprintln!(
            "⚠️ legacy config at {} is malformed, starting from defaults",
            legacy.display()
        );
        return;
    }
    match fs::write(target, raw) {
        Ok(()) => eprintln!(
            "⚙️ migrated trigger_config.json from {} to {}",
            legacy.display(),
            target.display()
        ),
        Err(e) => eprintln!("⚠️ config migration failed ({}), starting from defaults", e),
    }
}

fn write_config(cfg: &TriggerConfig) {
    if let Some(path) = config_path() {
        if let Ok(t) = serde_json::to_string_pretty(cfg) {
            if let Err(e) = fs::write(&path, t) {
                eprintln!("⚠️ could not write {}: {}", path.display(), e);
            }
        }
    }
}

/// Silent on purpose — the dispatcher reloads this on every capture, and a
/// println per keystroke drowns the log.
fn load_config() -> TriggerConfig {
    let Some(path) = config_path() else {
        return TriggerConfig::default();
    };

    // Cheap `exists()` stat on the hot path; a no-op once the file is present.
    migrate_legacy_config(&path);

    if let Ok(raw) = fs::read_to_string(&path) {
        match serde_json::from_str::<TriggerConfig>(&raw) {
            Ok(cfg) => {
                let (cfg, rewritten) = cfg.migrated();
                let cfg = cfg.sanitized();
                if rewritten {
                    eprintln!("⚙️ trigger_config.json migrated to v{}", CONFIG_VERSION);
                    write_config(&cfg);
                }
                return cfg;
            }
            Err(e) => {
                eprintln!("⚠️ trigger_config.json malformed ({}), restoring template.", e);
                let cfg = TriggerConfig::default();
                write_config(&cfg);
                return cfg;
            }
        }
    }

    let cfg = TriggerConfig::default();
    write_config(&cfg);
    cfg
}

#[tauri::command]
fn get_trigger_config() -> TriggerConfig {
    load_config()
}

#[tauri::command]
fn reload_trigger_config(app: AppHandle) -> TriggerConfig {
    let cfg = load_config();
    #[cfg(debug_assertions)]
    println!("⚙️ trigger config reloaded: {:?}", cfg);
    let _ = app.emit("trigger-config", &cfg);
    cfg
}

#[tauri::command]
fn reset_trigger_config(app: AppHandle) -> TriggerConfig {
    let cfg = TriggerConfig::default();
    write_config(&cfg);
    let _ = app.emit("trigger-config", &cfg);
    cfg
}

/// Exposed so the HUD can show the real path instead of guessing.
#[tauri::command]
fn get_trigger_config_path() -> Option<String> {
    config_path().map(|p| p.display().to_string())
}

// ---------------------------------------------------------------------------
// Window visibility and activation
// ---------------------------------------------------------------------------

/// `focusable: false` covers creation, but the style is re-applied after every
/// show because a window that was hidden can come back activation-hungry.
#[cfg(target_os = "windows")]
fn suppress_activation(window: &tauri::WebviewWindow) {
    if let Ok(h) = window.hwnd() {
        let hwnd = HWND(h.0 as _);
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | (WS_EX_NOACTIVATE.0 as isize));
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn suppress_activation(_window: &tauri::WebviewWindow) {}

/// OS-level hide/show. Used by the strip button and by the toggle key, so both
/// paths behave identically. Returns the resulting visibility.
///
/// The strip button must not rely on a click firing after `pointerdown` starts
/// a native drag — once the drag owns the pointer, the click never arrives and
/// the window stays collapsed. Hiding at the OS level sidesteps that entirely.
fn toggle_window_visibility(app: &AppHandle) -> bool {
    let Some(window) = app.get_webview_window("main") else {
        return false;
    };

    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        false
    } else {
        let _ = window.show();
        // No set_focus here: activation is what trips the exam's tab counter.
        suppress_activation(&window);
        true
    }
}

#[tauri::command]
fn toggle_overlay(app: AppHandle) -> bool {
    toggle_window_visibility(&app)
}

/// Lends the window real keyboard focus for a text field. The exam's current
/// foreground window is remembered first.
#[tauri::command]
fn begin_text_input(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    unsafe {
        PREV_FOREGROUND.store(GetForegroundWindow().0 as isize, Ordering::SeqCst);
    }

    let window = app.get_webview_window("main").ok_or("window_missing")?;
    window.set_focusable(true).map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

/// Drops focus again and pushes activation back to whatever had it before.
#[tauri::command]
fn end_text_input(app: AppHandle) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("window_missing")?;
    window.set_focusable(false).map_err(|e| e.to_string())?;
    suppress_activation(&window);

    #[cfg(target_os = "windows")]
    unsafe {
        let prev = PREV_FOREGROUND.swap(0, Ordering::SeqCst);
        if prev != 0 {
            let _ = SetForegroundWindow(HWND(prev as _));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

fn grab_primary_monitor() -> Result<String, String> {
    let screens = Screen::all().map_err(|e| e.to_string())?;
    let screen = screens.first().ok_or("monitor_not_found")?;
    let image = screen.capture().map_err(|e| e.to_string())?;

    let mut buffer = Vec::new();
    let mut cursor = Cursor::new(&mut buffer);
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut cursor, image::ImageOutputFormat::Jpeg(75))
        .map_err(|e| e.to_string())?;

    Ok(STANDARD.encode(&buffer))
}

/// Full-monitor capture for the bare-metal fallback.
///
/// The overlay is protected with WDA_EXCLUDEFROMCAPTURE and GDI BitBlt (what
/// the `screenshots` crate uses) renders protected windows as a solid black
/// rectangle. Hiding the window is used rather than dropping the protection,
/// because the overlay usually sits on top of the exam and would otherwise
/// cover the very text being captured. DWM needs a beat to recomposite, and the
/// first grab after a visibility change is frequently stale, so the shot is
/// taken twice and the first discarded.
#[tauri::command]
async fn capture_screen_native(app: AppHandle) -> Result<String, String> {
    let window = app.get_webview_window("main");
    if let Some(w) = &window {
        let _ = w.hide();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let result = tokio::task::spawn_blocking(|| {
        let _ = grab_primary_monitor();
        std::thread::sleep(Duration::from_millis(60));
        grab_primary_monitor()
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    // Restore on both paths — the old `?` skipped this on error and left the
    // overlay hidden.
    if let Some(w) = &window {
        let _ = w.show();
        suppress_activation(w);
    }

    result
}

/// Console-only capture for VM mode. `ip` doubles as a window hint.
///
/// The console window is excluded from capture before every grab so a proctor's
/// screen share never sees the VM. PrintWindow renders excluded windows fine,
/// so this costs nothing.
#[tauri::command]
async fn capture_rdp_window(ip: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "windows")]
        if let Ok(hwnd) = rdp::stream::find_console_window(&ip) {
            unsafe {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
        }
        rdp::stream::capture_window_by_hint(&ip)
    })
    .await
    .map_err(|e| format!("capture task panicked: {}", e))?
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Frame dump
//
// Bulk image export without a download prompt per file. The frontend sends the
// stored data URL as-is; the prefix is stripped here and the payload decoded
// straight to disk. Callers batch the work so no single IPC payload grows
// unbounded.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FrameDumpFile {
    /// Filename stem; the extension is added here.
    name: String,
    /// The full `data:image/jpeg;base64,...` URL as stored.
    data: String,
}

/// Keep a filename to characters Windows will accept.
fn sanitize_stem(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Guard against reserved trailing dots and empty stems.
    let trimmed = cleaned.trim_matches('.').to_string();
    if trimmed.is_empty() {
        "frame".to_string()
    } else {
        trimmed
    }
}

/// Create a fresh dump directory under the user's Downloads and return it.
#[tauri::command]
fn begin_frame_dump() -> Result<String, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let home = std::env::var("USERPROFILE").map_err(|e| format!("no USERPROFILE: {}", e))?;
    let dir = PathBuf::from(home)
        .join("Downloads")
        .join(format!("protocol_dump_{}", stamp));

    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    Ok(dir.to_string_lossy().to_string())
}

/// Decode and write a batch of frames into `dir`. Returns how many landed.
#[tauri::command]
fn write_frame_dump(dir: String, files: Vec<FrameDumpFile>) -> Result<usize, String> {
    let dir = PathBuf::from(dir);
    let mut written = 0usize;

    for file in files {
        let payload = file.data.strip_prefix(DATA_URL_PREFIX).unwrap_or(&file.data);
        let bytes = STANDARD
            .decode(payload)
            .map_err(|e| format!("decode '{}': {}", file.name, e))?;

        let path = dir.join(format!("{}.jpg", sanitize_stem(&file.name)));
        fs::write(&path, bytes).map_err(|e| format!("write {}: {}", path.display(), e))?;
        written += 1;
    }

    Ok(written)
}

/// Reveal a directory in Explorer. Explorer is GUI-subsystem, so the window is
/// the point — it takes no console flag.
#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open {}: {}", path, e))
}

// ---------------------------------------------------------------------------
// C2 endpoints
// ---------------------------------------------------------------------------

/// Starts provisioning and returns immediately. Progress is reported by
/// `vm-provisioned` / `vm-provision-failed` events and by polling
/// `get_vm_status`, so the overlay never sits on a frozen "please wait".
#[tauri::command]
async fn launch_stealth_vm(app: AppHandle, target: String) -> Result<String, String> {
    if PROVISIONING.swap(true, Ordering::SeqCst) {
        return Ok("provisioning already in flight".to_string());
    }

    #[cfg(debug_assertions)]
    println!("orchestrating hyper-v container for target: {}", target);

    let vm_name = std::env::var("PROTOCOL_VM_NAME").unwrap_or_else(|_| DEFAULT_VM_NAME.to_string());
    let spawn_name = vm_name.clone();
    let profile = target.clone();
    let handle = app.clone();

    tokio::task::spawn_blocking(move || {
        let outcome = vm::spawn_stealth_vm_blocking(spawn_name, Some(profile));
        PROVISIONING.store(false, Ordering::SeqCst);

        match outcome {
            Ok(report) if report.success => {
                let _ = handle.emit("vm-provisioned", report.message);
            }
            Ok(report) => {
                let _ = handle.emit("vm-provision-failed", report.message);
            }
            Err(e) => {
                let _ = handle.emit("vm-provision-failed", e.to_string());
            }
        }
    });

    Ok(format!("provisioning '{}'", vm_name))
}

// ---------------------------------------------------------------------------
// Keystroke engine
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Jittered cooldown ceiling for gated commands.
fn next_gate(cfg: &TriggerConfig) -> u64 {
    let span = cfg.cooldown_max_ms.saturating_sub(cfg.cooldown_min_ms).max(1);
    let mut x = now_ms() | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    now_ms() + cfg.cooldown_min_ms + (x % span)
}

fn key_token(k: &Key) -> Option<String> {
    let name = format!("{:?}", k);
    match k {
        Key::Tab => Some("Tab".to_string()),
        Key::CapsLock => Some("CapsLock".to_string()),
        Key::ScrollLock => Some("ScrollLock".to_string()),
        Key::Return | Key::KpReturn => Some("Return".to_string()),
        Key::Minus | Key::KpMinus => Some("-".to_string()),
        Key::Dot | Key::KpDelete => Some(".".to_string()),
        Key::Equal | Key::KpPlus => Some("=".to_string()),
        Key::LeftBracket => Some("[".to_string()),
        Key::RightBracket => Some("]".to_string()),
        Key::BackQuote => Some("`".to_string()),
        Key::SemiColon => Some(";".to_string()),
        Key::Comma => Some(",".to_string()),
        Key::Slash => Some("/".to_string()),
        Key::BackSlash => Some("\\".to_string()),
        _ => {
            if name.len() <= 8 {
                Some(name)
            } else {
                None
            }
        }
    }
}

enum KeyMsg {
    Press { key: Key, at: u64 },
    Release { key: Key },
}

fn keystroke_thread(app: AppHandle, rx: Receiver<KeyMsg>) {
    let mut cfg = load_config();
    #[cfg(debug_assertions)]
    {
        if let Some(p) = config_path() {
            println!("⚙️ config path: {}", p.display());
        }
        println!("⚙️ trigger config loaded: {:?}", cfg);
    }
    let _ = app.emit("trigger-config", &cfg);

    // Only gated commands appear here.
    let mut gates: HashMap<&'static str, u64> = HashMap::new();
    let mut last_key: Option<(Key, u64)> = None;

    // Currently-held keys and when last seen. rdev repeats KeyPress at the OS
    // repeat rate while a key is down; without this a held key fires on every
    // repeat. Small Vec beats a HashSet here and avoids a Hash bound on `Key`.
    let mut held: Vec<(Key, u64)> = Vec::new();

    let matches = |token: &Option<String>, trigger: &str| -> bool {
        token
            .as_ref()
            .map(|t| t.eq_ignore_ascii_case(trigger))
            .unwrap_or(false)
    };

    while let Ok(msg) = rx.recv() {
        let (key, at) = match msg {
            KeyMsg::Release { key } => {
                held.retain(|(k, _)| *k != key);
                continue;
            }
            KeyMsg::Press { key, at } => (key, at),
        };

        // Drop auto-repeat: same key still down and seen recently.
        if let Some(idx) = held.iter().position(|(k, _)| *k == key) {
            let seen = held[idx].1;
            held[idx].1 = at;
            if at.saturating_sub(seen) < REPEAT_GRACE_MS {
                continue;
            }
        } else {
            held.push((key, at));
        }

        let token = key_token(&key);

        let pair = matches!(&last_key, Some((prev, prev_at))
            if *prev == key && at.saturating_sub(*prev_at) <= cfg.double_tap_window_ms);

        if !pair {
            last_key = Some((key, at));
            continue;
        }

        let command: Option<&'static str> = if matches(&token, &cfg.summon_double_tap) {
            Some("SUMMON_COPILOT")
        } else if matches(&token, &cfg.sniper_double_tap) {
            Some("TRIGGER_SNIPER")
        } else if matches(&token, &cfg.flush_double_tap) {
            Some("FLUSH_BUFFER")
        } else if matches(&token, &cfg.audio_double_tap) {
            Some("TRIGGER_AUDIO")
        } else if matches(&token, &cfg.toggle_double_tap) {
            Some("TOGGLE_VISIBILITY")
        } else if matches(&token, &cfg.opacity_up_double_tap) {
            Some("OPACITY_UP")
        } else if matches(&token, &cfg.opacity_down_double_tap) {
            Some("OPACITY_DOWN")
        } else {
            None
        };

        let Some(command) = command else {
            last_key = Some((key, at));
            continue;
        };

        // Only the sniper is gated. Summon and toggle must feel instant.
        let gated = matches!(command, "TRIGGER_SNIPER");
        if gated {
            let allowed = gates.get(command).copied().unwrap_or(0);
            if at < allowed {
                #[cfg(debug_assertions)]
                println!("⏳ gate: {} blocked (retry after {})", command, allowed);
                // Re-arm with this press so the next tap retries immediately
                // instead of the attempt being swallowed.
                last_key = Some((key, at));
                continue;
            }
            gates.insert(command, next_gate(&cfg));
        }

        #[cfg(debug_assertions)]
        println!("🔥 INTERCEPT: {}", command);

        // Both paths go through the same OS-level helper, so the key and the
        // strip button can never disagree about visibility.
        if command == "SUMMON_COPILOT" || command == "TOGGLE_VISIBILITY" {
            toggle_window_visibility(&app);
        }

        if command == "TRIGGER_SNIPER" || command == "TRIGGER_AUDIO" {
            // Re-read so edits to the JSON take effect without a restart.
            cfg = load_config();
        }

        if let Err(e) = app.emit("app-command", command) {
            #[cfg(debug_assertions)]
            eprintln!("⚠️ IPC warning - frontend unhooked: {}", e);
        }

        last_key = None;
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .invoke_handler(tauri::generate_handler![
            capture_screen_native,
            capture_rdp_window,
            begin_frame_dump,
            write_frame_dump,
            open_folder,
            get_trigger_config,
            get_trigger_config_path,
            reload_trigger_config,
            reset_trigger_config,
            toggle_overlay,
            begin_text_input,
            end_text_input,
            launch_stealth_vm,
            target_id::scan_for_lockdown_browsers,
            vm::get_vm_status,
            vm::spawn_stealth_vm_cmd,
            vm::shutdown_vm,
            rdp::get_tunnel_status,
            rdp::start_loopback_tunnel_cmd,
            rdp::stop_loopback_tunnel,
            patches::apply_target_patches,
            stealth::verify_stealth_posture,
        ])
        .setup(|app| {
            // Raw global keystream — replaces every global shortcut.
            let app_handle = app.handle().clone();
            let (tx, rx) = channel::<KeyMsg>();

            std::thread::spawn(move || keystroke_thread(app_handle, rx));

            std::thread::spawn(move || {
                let callback = move |event: rdev::Event| {
                    match event.event_type {
                        EventType::KeyPress(key) => {
                            let _ = tx.send(KeyMsg::Press { key, at: now_ms() });
                        }
                        EventType::KeyRelease(key) => {
                            let _ = tx.send(KeyMsg::Release { key });
                        }
                        _ => {}
                    }
                };
                if let Err(e) = listen(callback) {
                    eprintln!("⚠️ keystroke listener failed to attach: {:?}", e);
                }
            });

            #[cfg(target_os = "windows")]
            {
                if let Some(window) = app.get_webview_window("main") {
                    if let Ok(hwnd_wrapper) = window.hwnd() {
                        let hwnd = HWND(hwnd_wrapper.0 as _);
                        unsafe {
                            // Belt and braces on top of `focusable: false`. If
                            // either path ever regresses, the other still holds.
                            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                            SetWindowLongPtrW(
                                hwnd,
                                GWL_EXSTYLE,
                                style | (WS_EX_NOACTIVATE.0 as isize),
                            );

                            if SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_err() {
                                eprintln!("🛡️ stealth failure: os blocked drm evasion.");
                            }
                        }
                        #[cfg(debug_assertions)]
                        println!("🛡️ DRM screen evasion active, activation suppressed.");
                    }
                }

                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    tauri::async_runtime::block_on(target_id::start_detection_loop(&handle));
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("CRITICAL FAILURE: OS rejected application initialization.");
}