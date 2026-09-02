#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// 🛡️ CRITICAL: Emitter for IPC, Manager for OS Window control
use tauri::{Emitter, Manager}; 
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

// 📸 Native Capture Dependencies
use screenshots::Screen;
use std::io::Cursor;
use base64::{Engine as _, engine::general_purpose::STANDARD};

// 🛡️ DRM TARGETS: Only compile Windows APIs on Windows machines
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;


// 📸 THE SILENT SCREEN CAPTURE COMMAND
#[tauri::command]
async fn capture_screen_native() -> Result<String, String> {
    // 1. Detect monitors natively (No browser popups!)
    let screens = Screen::all().map_err(|e| e.to_string())?;
    let screen = screens.first().ok_or("No monitor detected")?;
    
    // 2. Snap the screen
    let image = screen.capture().map_err(|e| e.to_string())?;
    
    // 3. Compress heavily into JPEG in-memory for lightning-fast network upload
    let mut buffer = Vec::new();
    let mut cursor = Cursor::new(&mut buffer);
    let dynamic_image = image::DynamicImage::ImageRgba8(image);
    dynamic_image.write_to(&mut cursor, image::ImageOutputFormat::Jpeg(75)).map_err(|e| e.to_string())?;
    
    // 4. Encode to Base64 and return to React
    Ok(STANDARD.encode(&buffer))
}


fn main() {
    tauri::Builder::default()
        // 1. Native Window Tracker (Remembers position and size across reboots)
        .plugin(tauri_plugin_window_state::Builder::default().build())
        
        // 2. Global Macro Listener
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        let raw_os_key = shortcut.to_string().to_lowercase();
                        
                        // 🛡️ The Summoning Protocol
                        let unified_command = match raw_os_key.as_str() {
                            k if k.contains("alt+-") => "SUMMON_COPILOT",
                            k if k.contains("alt+t") => "SUMMON_COPILOT", 
                            k if k.contains("keya") => "TRIGGER_AUDIO",
                            k if k.contains("keyh") => "TOGGLE_VISIBILITY",
                            k if k.contains("arrowup") => "OPACITY_UP",
                            k if k.contains("arrowdown") => "OPACITY_DOWN",
                            _ => "UNKNOWN_COMMAND",
                        };

                        #[cfg(debug_assertions)]
                        println!("🔥 OS INTERCEPT: {}", unified_command);

                        // 🛡️ GOD-TIER UX: Zero-Latency OS Window Summoning
                        if unified_command == "SUMMON_COPILOT" || unified_command == "TOGGLE_VISIBILITY" {
                            if let Some(window) = app.get_webview_window("main") {
                                if unified_command == "TOGGLE_VISIBILITY" {
                                    let is_visible = window.is_visible().unwrap_or(false);
                                    if is_visible {
                                        let _ = window.hide();
                                    } else {
                                        let _ = window.show();
                                        let _ = window.set_focus();
                                    }
                                } else {
                                    // For SUMMON_COPILOT, force the window open and steal focus
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            }
                        }

                        // 🛡️ DEFENSIVE EMISSION: Route command to React
                        if let Err(e) = app.emit("app-command", unified_command) {
                            #[cfg(debug_assertions)]
                            eprintln!("⚠️ IPC Warning - Frontend unhooked: {}", e);
                        }
                    }
                })
                .build(),
        )
        // 👇 BIND THE NATIVE CAPTURE COMMAND HERE 👇
        .invoke_handler(tauri::generate_handler![capture_screen_native])
        .setup(|app| {
            // 3. Register Hotkeys Gracefully
            let _ = app.global_shortcut().register("Alt+-");
            let _ = app.global_shortcut().register("Alt+T");
            let _ = app.global_shortcut().register("Alt+A");
            let _ = app.global_shortcut().register("Alt+H");
            let _ = app.global_shortcut().register("Alt+Up");
            let _ = app.global_shortcut().register("Alt+Down");

            // 4. 🛡️ GOD-TIER FEATURE: OS-Level Screen Capture Evasion
            #[cfg(target_os = "windows")]
            {
                if let Some(window) = app.get_webview_window("main") {
                    if let Ok(hwnd_wrapper) = window.hwnd() {
                        let hwnd = hwnd_wrapper.0 as isize;
                        
                        unsafe {
                            let _ = SetWindowDisplayAffinity(
                                HWND(hwnd as _), 
                                WDA_EXCLUDEFROMCAPTURE
                            );
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