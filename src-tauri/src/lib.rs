use tauri::Emitter; // 👈 CRITICAL: Required to send events to React
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        let shortcut_str = shortcut.to_string();
                        // This line requires the Emitter trait!
                        app.emit("hotkey-pressed", shortcut_str).unwrap();
                    }
                })
                .build(),
        )
        .setup(|app| {
            // app.global_shortcut().register("Alt+S")?; // <--- REMOVED
            app.global_shortcut().register("Alt+H")?;
            app.global_shortcut().register("Alt+Up")?;
            app.global_shortcut().register("Alt+Down")?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}