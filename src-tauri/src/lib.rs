mod hyper_v;
mod patches;
mod rdp;
mod stealth;
mod target_id;
mod util;
mod vm;

pub use target_id::{fetch_all, fetch_profile};

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
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
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};

/// Fallback guest name when `PROTOCOL_VM_NAME` is unset.
const DEFAULT_VM_NAME: &str = "Test-Nartan-Build";

/// A key still flagged as held after this long is assumed to have lost its
/// release event (focus steal, hook drop) and is treated as fresh.
const REPEAT_GRACE_MS: u64 = 5_000;

// ---------------------------------------------------------------------------
// Trigger configuration (trigger_config.json next to the executable)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerConfig {
    /// Double-tap to summon the copilot. Default: CapsLock (invisible, net-zero).
    pub summon_double_tap: String,
    /// Double-tap to buffer one frame. Default: "-" (leaves "--").
    pub sniper_double_tap: String,
    /// Double-tap to flush the frame buffer. Default: Return.
    pub flush_double_tap: String,
    /// Double-tap to start/stop audio capture. Default: "." (leaves "..").
    pub audio_double_tap: String,
    /// Double-tap to hide/show the window. Default: ScrollLock (inert in browsers).
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
            summon_double_tap: "CapsLock".to_string(),
            sniper_double_tap: "-".to_string(),
            flush_double_tap: "Return".to_string(),
            audio_double_tap: ".".to_string(),
            toggle_double_tap: "ScrollLock".to_string(),
            opacity_up_double_tap: "=".to_string(),
            opacity_down_double_tap: "[".to_string(),
            double_tap_window_ms: 400,
            cooldown_min_ms: 800,
            cooldown_max_ms: 1200,
        }
    }
}

impl TriggerConfig {
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

        if self.double_tap_window_ms < 120 {
            self.double_tap_window_ms = 400;
        }
        if self.cooldown_max_ms < self.cooldown_min_ms {
            self.cooldown_max_ms = self.cooldown_min_ms;
        }
        self
    }
}

fn config_path() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .map(|d| d.join("trigger_config.json"))
}

/// Silent on purpose — the dispatcher reloads this on every capture, and a
/// println per keystroke drowns the log.
fn load_config() -> TriggerConfig {
    if let Some(path) = config_path() {
        if let Ok(raw) = fs::read_to_string(&path) {
            match serde_json::from_str::<TriggerConfig>(&raw) {
                Ok(cfg) => return cfg.sanitized(),
                Err(e) => {
                    eprintln!("⚠️ trigger_config.json malformed ({}), restoring template.", e);
                    if let Ok(t) = serde_json::to_string_pretty(&TriggerConfig::default()) {
                        let _ = fs::write(&path, t);
                    }
                }
            }
        } else if let Ok(t) = serde_json::to_string_pretty(&TriggerConfig::default()) {
            let _ = fs::write(&path, t);
        }
    }
    TriggerConfig::default()
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
/// The window is protected with WDA_EXCLUDEFROMCAPTURE, and GDI BitBlt (what
/// the `screenshots` crate uses) renders protected windows as a solid black
/// rectangle. Protection is dropped for the duration of the shot so the exam
/// underneath stays readable, then restored.
#[tauri::command]
async fn capture_screen_native(app: AppHandle) -> Result<String, String> {
    let window = app.get_webview_window("main");
    if let Some(w) = &window {
        let _ = w.set_content_protected(false);
    }
    tokio::time::sleep(Duration::from_millis(90)).await;

    let result = tokio::task::spawn_blocking(grab_primary_monitor)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(w) = &window {
        let _ = w.set_content_protected(true);
    }
    result
}

/// Console-only capture for VM mode. `ip` doubles as a window hint.
#[tauri::command]
async fn capture_rdp_window(ip: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || rdp::stream::capture_window_by_hint(&ip))
        .await
        .map_err(|e| format!("capture task panicked: {}", e))?
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// C2 endpoints
// ---------------------------------------------------------------------------

#[tauri::command]
async fn launch_stealth_vm(target: String) -> Result<String, String> {
    #[cfg(debug_assertions)]
    println!("orchestrating hyper-v container for target: {}", target);

    let vm_name = std::env::var("PROTOCOL_VM_NAME").unwrap_or_else(|_| DEFAULT_VM_NAME.to_string());
    let spawn_name = vm_name.clone();
    let profile = target.clone();

    // `spawn_stealth_vm_cmd` is async; the blocking form is what belongs inside
    // spawn_blocking.
    let spawned = tokio::task::spawn_blocking(move || {
        vm::spawn_stealth_vm_blocking(spawn_name, Some(profile))
    })
    .await
    .map_err(|e| format!("spawn task panicked: {}", e))?
    .map_err(|e| format!("spawn failed: {}", e))?;

    if !spawned.success {
        return Err(format!(
            "VM '{}' did not reach Running ({})",
            vm_name, spawned.message
        ));
    }

    Ok(format!(
        "hypervisor engaged. {} is now isolated in sandbox '{}'.",
        target, vm_name
    ))
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
    println!("⚙️ trigger config loaded: {:?}", cfg);
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

        if command == "SUMMON_COPILOT" || command == "TOGGLE_VISIBILITY" {
            if let Some(window) = app.get_webview_window("main") {
                if command == "TOGGLE_VISIBILITY" && window.is_visible().unwrap_or(false) {
                    let _ = window.hide();
                } else {
                    let _ = window.show();
                }
            }
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
            get_trigger_config,
            reload_trigger_config,
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
                        unsafe {
                            if SetWindowDisplayAffinity(
                                HWND(hwnd_wrapper.0 as _),
                                WDA_EXCLUDEFROMCAPTURE,
                            )
                            .is_err()
                            {
                                eprintln!("🛡️ stealth failure: os blocked drm evasion.");
                            }
                        }
                        #[cfg(debug_assertions)]
                        println!("🛡️ DRM screen evasion active.");
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