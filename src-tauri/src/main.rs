#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// 🛡️ CRITICAL: Emitter for IPC, Manager for OS Window control
use tauri::{AppHandle, Emitter, Manager};

// 📸 Native Capture Dependencies
use screenshots::Screen;
use std::io::Cursor;
use base64::{Engine as _, engine::general_purpose::STANDARD};

// 🎹 Raw global keystroke listener (replaces tauri-plugin-global-shortcut entirely)
use rdev::{listen, EventType, Key};
use std::sync::mpsc::{channel, Receiver};
use std::time::{SystemTime, UNIX_EPOCH};

// ⚙️ Config-driven triggers
use serde::{Deserialize, Serialize};
use std::fs;

// 🛡️ DRM TARGETS: Only compile Windows APIs on Windows machines
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;

// ─────────────────────────────────────────────────────────────
// ⚙️ TRIGGER CONFIGURATION (trigger_config.json next to the .exe)
// ─────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerConfig {
    /// Double-tap key for summoning the copilot window. "Tab" = Tab+Tab.
    pub summon_double_tap: String,
    /// Double-tap key for the sniper capture. "-" = -+-.
    pub sniper_double_tap: String,
    /// Double-tap key for visibility toggle. "." = .+.
    pub toggle_double_tap: String,
    /// Max ms between two presses of the same key to count as a double-tap.
    pub double_tap_window_ms: u64,
    /// Cooldown FLOOR — never fire the same command again sooner than this.
    pub cooldown_min_ms: u64,
    /// Cooldown CEILING — jitter range. Next allowed time is random in
    /// [cooldown_min_ms, cooldown_max_ms]. This breaks the metronome:
    /// real humans pause irregularly, beacons fire on a clock.
    pub cooldown_max_ms: u64,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            summon_double_tap: "Tab".to_string(),
            sniper_double_tap: "-".to_string(),
            toggle_double_tap: ".".to_string(),
            double_tap_window_ms: 400,
            cooldown_min_ms: 25_000,
            cooldown_max_ms: 45_000,
        }
    }
}

impl TriggerConfig {
    fn load() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));

        if let Some(dir) = exe_dir {
            let path = dir.join("trigger_config.json");
            if let Ok(raw) = fs::read_to_string(&path) {
                match serde_json::from_str::<TriggerConfig>(&raw) {
                    Ok(cfg) => {
                        #[cfg(debug_assertions)]
                        println!("⚙️ trigger_config.json loaded: {:?}", cfg);
                        return cfg;
                    }
                    Err(e) => {
                        eprintln!("⚠️ trigger_config.json malformed ({}), using defaults + writing template.", e);
                        // Write a template back out so the user can fix it.
                        if let Ok(template) = serde_json::to_string_pretty(&TriggerConfig::default()) {
                            let _ = fs::write(&path, template);
                        }
                    }
                }
            } else {
                // No config yet — drop a template so per-user tuning is obvious.
                if let Ok(template) = serde_json::to_string_pretty(&TriggerConfig::default()) {
                    let _ = fs::write(&path, template);
                }
            }
        }
        TriggerConfig::default()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Randomized cooldown: uniform in [min, max). The variance is the whole point —
/// a fixed 30s interval is a timing beacon even with a "normal-looking" key pattern.
fn next_allowed_trigger(cfg: &TriggerConfig) -> u64 {
    let span = cfg.cooldown_max_ms.saturating_sub(cfg.cooldown_min_ms).max(1);
    // Cheap entropy: mix the clock's sub-millisecond precision with an xorshift state.
    use std::cell::Cell;
    thread_local! {
        static RNG_STATE: Cell<u64> = Cell::new(0);
    }
    let jitter = RNG_STATE.with(|s| {
        let mut x = s.get();
        if x == 0 {
            x = now_ms() | 1;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x % span as u64) as u64
    });
    now_ms() + cfg.cooldown_min_ms + jitter
}

// ─────────────────────────────────────────────────────────────
// 📸 THE SILENT SCREEN CAPTURE COMMAND
// ─────────────────────────────────────────────────────────────
#[tauri::command]
async fn capture_screen_native() -> Result<String, String> {
    // use a task block to keep the executor snappy
    tokio::task::spawn_blocking(move || {
        let screens = Screen::all().map_err(|e| e.to_string())?;
        // capture primary screen (usually index 0)
        let screen = screens.first().ok_or("monitor_not_found")?;

        let image = screen.capture().map_err(|e| e.to_string())?;
        let mut buffer = Vec::new();
        let mut cursor = Cursor::new(&mut buffer);

        // convert to dynamic image for processing
        let dynamic_image = image::DynamicImage::ImageRgba8(image);

        // 75 quality is the sweet spot for reading text while keeping payloads < 200kb
        dynamic_image
            .write_to(&mut cursor, image::ImageOutputFormat::Jpeg(75))
            .map_err(|e| e.to_string())?;

        Ok(STANDARD.encode(&buffer))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ─────────────────────────────────────────────────────────────
// 🎹 RAW KEYSTREAM PROCESSOR (the noise-floor trigger engine)
// ─────────────────────────────────────────────────────────────
enum KeyMsg {
    Press { key: Key, at: u64 },
}

fn keystroke_thread(app: AppHandle, rx: Receiver<KeyMsg>) {
    let cfg = TriggerConfig::load();

    // Per-command cooldown state: command -> next-allowed timestamp.
    let mut cooldowns: std::collections::HashMap<&'static str, u64> =
        std::collections::HashMap::new();
    // Double-tap tracker: last key pressed + when.
    let mut last_key: Option<(Key, u64)> = None;

    // Map raw rdev keys to the config's trigger tokens (case-insensitive compare
    // on the debug name, falling back to common layout aliases).
    let key_token = |k: &Key| -> Option<String> {
        let name = format!("{:?}", k);
        match k {
            Key::Tab => Some("Tab".to_string()),
            Key::Minus | Key::KpMinus => Some("-".to_string()),
            Key::Dot | Key::KpDelete => Some(".".to_string()),
            _ => {
                // allow config authors to reference keys by debug name too
                if name.len() <= 8 { Some(name) } else { None }
            }
        }
    };

    let matches_trigger = |token: &Option<String>, trigger: &str| -> bool {
        match token {
            Some(t) => t.eq_ignore_ascii_case(trigger),
            None => false,
        }
    };

    while let Ok(msg) = rx.recv() {
        let KeyMsg::Press { key, at } = msg;

        let token = key_token(&key);

        // ── Double-tap detection ──────────────────────────────
        // Same key twice inside the window = a "typing-noise" trigger.
        // "--", "..", Tab+Tab all look like hesitation to a keystroke logger.
        if let Some((prev_key, prev_at)) = last_key {
            if prev_key == key && at.saturating_sub(prev_at) <= cfg.double_tap_window_ms {
                let fired: Option<&'static str> = if matches_trigger(&token, &cfg.summon_double_tap)
                {
                    Some("SUMMON_COPILOT")
                } else if matches_trigger(&token, &cfg.sniper_double_tap) {
                    Some("TRIGGER_SNIPER")
                } else if matches_trigger(&token, &cfg.toggle_double_tap) {
                    Some("TOGGLE_VISIBILITY")
                } else {
                    None
                };

                if let Some(command) = fired {
                    // ── Jittered cooldown gate ──────────────────────
                    let allowed_from = cooldowns.get(command).copied().unwrap_or(0);
                    if at >= allowed_from {
                        cooldowns.insert(command, next_allowed_trigger(&cfg));

                        #[cfg(debug_assertions)]
                        println!("🔥 NOISE-FLOOR INTERCEPT: {} (raw double-tap)", command);

                        // 🛡️ Zero-Latency OS Window Summoning (same as before)
                        if command == "SUMMON_COPILOT" || command == "TOGGLE_VISIBILITY" {
                            if let Some(window) = app.get_webview_window("main") {
                                if command == "TOGGLE_VISIBILITY" {
                                    let is_visible = window.is_visible().unwrap_or(false);
                                    if is_visible {
                                        let _ = window.hide();
                                    } else {
                                        let _ = window.show();
                                        let _ = window.set_focus();
                                    }
                                } else {
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            }
                        }

                        // 🛡️ DEFENSIVE EMISSION: Route command to React
                        if let Err(e) = app.emit("app-command", command) {
                            #[cfg(debug_assertions)]
                            eprintln!("⚠️ IPC Warning - Frontend unhooked: {}", e);
                        }
                    } else {
                        #[cfg(debug_assertions)]
                        println!("⏳ cooldown gate: {} blocked (retry after {})", command, allowed_from);
                    }

                    // Consume the pair so a triple-tap doesn't double-fire.
                    last_key = None;
                    continue;
                }
            }
        }

        last_key = Some((key, at));
    }
}

fn main() {
    tauri::Builder::default()
        // 1. Native Window Tracker (Remembers position and size across reboots)
        .plugin(tauri_plugin_window_state::Builder::default().build())
        // 👇 BIND THE NATIVE CAPTURE COMMAND HERE 👇
        .invoke_handler(tauri::generate_handler![capture_screen_native])
        .setup(|app| {
            // 2. 🎹 RAW KEYSTREAM LISTENER — replaces ALL global shortcuts.
            //    rdev hooks below the level proctoring tools log friendly
            //    accelerator names for; we see raw keys and decide ourselves.
            let app_handle = app.handle().clone();
            let (tx, rx) = channel::<KeyMsg>();

            std::thread::spawn(move || {
                keystroke_thread(app_handle, rx);
            });

            std::thread::spawn(move || {
                let callback = move |event: rdev::Event| {
                    if let EventType::KeyPress(key) = event.event_type {
                        let _ = tx.send(KeyMsg::Press { key, at: now_ms() });
                    }
                };
                if let Err(e) = listen(callback) {
                    eprintln!("⚠️ keystroke listener failed to attach: {:?}", e);
                }
            });

            // 3. 🛡️ GOD-TIER FEATURE: OS-Level Screen Capture Evasion
            #[cfg(target_os = "windows")]
            {
                if let Some(window) = app.get_webview_window("main") {
                    if let Ok(hwnd_wrapper) = window.hwnd() {
                        let hwnd = hwnd_wrapper.0 as isize;

                        unsafe {
                            let result = SetWindowDisplayAffinity(
                                HWND(hwnd as _),
                                WDA_EXCLUDEFROMCAPTURE,
                            );
                            if result.is_err() {
                                eprintln!("🛡️ stealth failure: os blocked drm evasion. check permissions.");
                            }
                        }

                        #[cfg(debug_assertions)]
                        println!("🛡️ Stealth Protocol: DRM Screen Evasion Active.");
                    }
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("CRITICAL FAILURE: OS rejected application initialization.");
}