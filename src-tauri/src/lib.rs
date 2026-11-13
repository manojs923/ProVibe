mod hyper_v;
mod patches;
mod rdp;
mod stealth;
mod target_id;
mod util;
mod vm;

pub use target_id::{fetch_all, fetch_profile};

use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// Fallback guest name when `PROTOCOL_VM_NAME` is unset.
const DEFAULT_VM_NAME: &str = "Test-Nartan-Build";

// ---------------------------------------------------------------------------
// C2 endpoints
// ---------------------------------------------------------------------------

/// Orchestrate a stealth VM for `target` and report the resulting state.
///
/// Spawning shells out to `vmconnect` and blocks, so it runs on the blocking
/// pool rather than stalling the async runtime.
#[tauri::command]
async fn launch_stealth_vm(target: String) -> Result<String, String> {
    #[cfg(debug_assertions)]
    println!("orchestrating hyper-v container for target: {}", target);

    let vm_name =
        std::env::var("PROTOCOL_VM_NAME").unwrap_or_else(|_| DEFAULT_VM_NAME.to_string());

    let spawn_name = vm_name.clone();
    let profile = target.clone();

    let spawned =
        tokio::task::spawn_blocking(move || vm::spawn_stealth_vm_cmd(spawn_name, Some(profile)))
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

/// Capture the guest console as a base64 JPEG.
///
/// `ip` doubles as a window hint: the best-scoring visible console window wins,
/// so an IP that matches no title still resolves the only open console.
#[tauri::command]
async fn capture_rdp_window(ip: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || rdp::stream::capture_window_by_hint(&ip))
        .await
        .map_err(|e| format!("capture task panicked: {}", e))?
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Windows-only imports stay inside `run` so the module compiles elsewhere.
    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::HWND;
    #[cfg(target_os = "windows")]
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };

    tauri::Builder::default()
        // 1. Native window tracker (remembers position and size across reboots).
        .plugin(tauri_plugin_window_state::Builder::default().build())
        // 2. Global macro listener.
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }

                    let raw_os_key = shortcut.to_string().to_lowercase();

                    // The summoning protocol.
                    let unified_command = match raw_os_key.as_str() {
                        k if k.contains("alt+-") => "SUMMON_COPILOT",
                        k if k.contains("alt+t") => "SUMMON_COPILOT",
                        k if k.contains("keya") => "TRIGGER_AUDIO",
                        k if k.contains("keyh") => "TOGGLE_VISIBILITY",
                        k if k.contains(".") || k.contains("period") => "TOGGLE_VISIBILITY",
                        k if k.contains("arrowup") => "OPACITY_UP",
                        k if k.contains("arrowdown") => "OPACITY_DOWN",
                        k if k.contains("keyv") => "SPAWN_VM",
                        k if k.contains("keyr") => "TOGGLE_RDP_TUNNEL",
                        _ => "UNKNOWN_COMMAND",
                    };

                    #[cfg(debug_assertions)]
                    println!("OS INTERCEPT: {}", unified_command);

                    // Zero-latency OS window summoning.
                    if unified_command == "SUMMON_COPILOT"
                        || unified_command == "TOGGLE_VISIBILITY"
                    {
                        if let Some(window) = app.get_webview_window("main") {
                            if unified_command == "TOGGLE_VISIBILITY"
                                && window.is_visible().unwrap_or(false)
                            {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }

                    // VM spawn hotkey routes into the orchestration subsystem.
                    if unified_command == "SPAWN_VM" {
                        std::thread::spawn(move || {
                            let vm_name = std::env::var("PROTOCOL_VM_NAME")
                                .unwrap_or_else(|_| DEFAULT_VM_NAME.to_string());

                            if let Err(e) = vm::spawn_stealth_vm_cmd(vm_name, None) {
                                #[cfg(debug_assertions)]
                                eprintln!("VM spawn failed: {}", e);
                            }
                        });
                    }

                    // RDP loopback tunnel toggle — stops when live, starts when not.
                    if unified_command == "TOGGLE_RDP_TUNNEL" {
                        std::thread::spawn(move || {
                            let active = rdp::get_tunnel_status(None)
                                .map(|status| status.active)
                                .unwrap_or(false);

                            let outcome = if active {
                                rdp::stop_loopback_tunnel(None)
                            } else {
                                rdp::start_loopback_tunnel_cmd(None, None)
                                    .map(|result| result.message)
                            };

                            if let Err(e) = outcome {
                                #[cfg(debug_assertions)]
                                eprintln!("RDP tunnel toggle failed: {}", e);
                            }
                        });
                    }

                    // Defensive event emission — the frontend may not be up yet.
                    if let Err(e) = app.emit("app-command", unified_command) {
                        #[cfg(debug_assertions)]
                        eprintln!("WARNING: frontend unreachable for hotkey event: {}", e);
                    }
                })
                .build(),
        )
        .setup(|app| {
            // 3. Register global shortcuts.
            let _ = app.global_shortcut().register("Alt+-");
            let _ = app.global_shortcut().register("Alt+T");
            let _ = app.global_shortcut().register("Alt+A");
            let _ = app.global_shortcut().register("Alt+H");
            let _ = app.global_shortcut().register("Alt+Up");
            let _ = app.global_shortcut().register("Alt+Down");
            let _ = app.global_shortcut().register(".");
            let _ = app.global_shortcut().register("Alt+V");
            let _ = app.global_shortcut().register("Alt+R");

            // 4. OS-level screen-capture evasion.
            #[cfg(target_os = "windows")]
            {
                if let Some(window) = app.get_webview_window("main") {
                    if let Ok(hwnd_wrapper) = window.hwnd() {
                        let hwnd = hwnd_wrapper.0 as isize;
                        unsafe {
                            let _ = SetWindowDisplayAffinity(
                                HWND(hwnd as *mut core::ffi::c_void),
                                WDA_EXCLUDEFROMCAPTURE,
                            );
                        }
                        println!("Stealth protocol: window is now invisible to screen capture.");
                    }
                }
            }

            // 5. Passive lockdown-browser detection at startup.
            #[cfg(target_os = "windows")]
            {
                let app_handle = app.handle().clone();
                std::thread::spawn(move || {
                    target_id::start_detection_loop(app_handle);
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            target_id::scan_for_lockdown_browsers,
            vm::get_vm_status,
            vm::spawn_stealth_vm_cmd,
            vm::shutdown_vm,
            rdp::get_tunnel_status,
            rdp::start_loopback_tunnel_cmd,
            rdp::stop_loopback_tunnel,
            patches::apply_target_patches,
            stealth::verify_stealth_posture,
            launch_stealth_vm,
            capture_rdp_window,
        ])
        .run(tauri::generate_context!())
        .expect("CRITICAL FAILURE: The Operating System refused to allocate resources for the application.");
}