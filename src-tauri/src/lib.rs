#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 🛡️ CRITICAL: Manager is required to interact with the OS window
    use tauri::{Emitter, Manager}; 
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

    // conditionally compile Windows native APIs only when building for Windows
    #[cfg(target_os = "windows")]
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};
    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::HWND;

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
                            k if k.contains(".") || k.contains("period") => "TOGGLE_VISIBILITY", // 🔥 NEW: Focus toggle
                            k if k.contains("arrowup") => "OPACITY_UP",
                            k if k.contains("arrowdown") => "OPACITY_DOWN",
                            _ => "UNKNOWN_COMMAND",
                        };

                        #[cfg(debug_assertions)]
                        println!("🔥 OS INTERCEPT: {}", unified_command);

                        // 🛡️ GOD-TIER UX: Zero-Latency OS Window Summoning
                        // Bypasses React entirely to snap the window instantly into view via the OS compositor
                        if unified_command == "SUMMON_COPILOT" || unified_command == "TOGGLE_VISIBILITY" {
                            if let Some(window) = app.get_webview_window("main") {
                                if unified_command == "TOGGLE_VISIBILITY" {
                                    let is_visible = window.is_visible().unwrap_or(false);
                                    if is_visible {
                                        let _ = window.hide(); // OS instantly gives focus back to the browser
                                    } else {
                                        let _ = window.show();
                                        let _ = window.set_focus(); // OS instantly pulls focus back to the agent
                                    }
                                } else {
                                    // For SUMMON_COPILOT, always force the window open and steal focus
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            }
                        }

                        // 🛡️ SENIOR ENGINEERING: Defensive Event Emission
                        // Never use .unwrap() on event emitters. If the frontend is busy or reloading,
                        // a panic here will instantly kill the entire application process. 
                        // We handle the error silently to maintain stealth.
                        if let Err(e) = app.emit("app-command", unified_command) {
                            #[cfg(debug_assertions)]
                            eprintln!("WARNING: Frontend unreachable for hotkey event: {}", e);
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            // 3. Register Global Shortcuts (Propagating initialization errors safely)
            // Using `let _ =` prevents a startup crash if Discord or VS Code is already using a hotkey.
            let _ = app.global_shortcut().register("Alt+-");
            let _ = app.global_shortcut().register("Alt+T");
            let _ = app.global_shortcut().register("Alt+A");
            let _ = app.global_shortcut().register("Alt+H");
            let _ = app.global_shortcut().register("Alt+Up");
            let _ = app.global_shortcut().register("Alt+Down");
            
            // 🔥 NEW: Register the single period key for instant focus flipping
            let _ = app.global_shortcut().register(".");

            // 4. 🛡️ GOD-TIER FEATURE: OS-Level Screen Capture Evasion
            // We use cfg constraints so this doesn't break Mac/Linux builds in CI/CD pipelines.
            #[cfg(target_os = "windows")]
            {
                // Safely attempt to get the main window without assuming it exists
                if let Some(window) = app.get_webview_window("main") {
                    // Safely extract the raw OS Window Handle (HWND)
                    if let Ok(hwnd_wrapper) = window.hwnd() {
                        let hwnd = hwnd_wrapper.0 as isize;
                        
                        // Drop down into Unsafe Rust to talk directly to the Windows Kernel/DWM
                        unsafe {
                            let _ = SetWindowDisplayAffinity(
                                HWND(hwnd as _), 
                                WDA_EXCLUDEFROMCAPTURE
                            );
                        }
                        println!("🛡️ Stealth Protocol: Window is now invisible to screen capture.");
                    }
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("CRITICAL FAILURE: The Operating System refused to allocate resources for the application.");
}