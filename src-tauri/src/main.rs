#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Emitter; 
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        let raw_os_key = shortcut.to_string().to_lowercase();
                        
                        // The CTO Translation Layer
                        let unified_command = match raw_os_key.as_str() {
                            k if k.contains("keys") => "TRIGGER_SNIPER",
                            k if k.contains("keya") => "TRIGGER_AUDIO", // 👈 NEW: Audio Hotkey
                            k if k.contains("keyh") => "TOGGLE_VISIBILITY",
                            k if k.contains("arrowup") => "OPACITY_UP",
                            k if k.contains("arrowdown") => "OPACITY_DOWN",
                            _ => "UNKNOWN_COMMAND",
                        };

                        println!("🔥 TRANSLATED COMMAND: {}", unified_command);
                        app.emit("app-command", unified_command).unwrap();
                    }
                })
                .build(),
        )
        .setup(|app| {
            app.global_shortcut().register("Alt+S")?;
            app.global_shortcut().register("Alt+A")?; // 👈 NEW: Register Alt+A with Windows
            app.global_shortcut().register("Alt+H")?;
            app.global_shortcut().register("Alt+Up")?;
            app.global_shortcut().register("Alt+Down")?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}