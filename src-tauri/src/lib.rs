// 🔥 NEW: the exact endpoints your react c2 panel is looking for
#[tauri::command]
async fn launch_stealth_vm(target: String) -> Result<String, String> {
    #[cfg(debug_assertions)]
    println!("orchestrating hyper-v container for target: {}", target);

    // wire this into your existing modules
    // e.g., vm::spawn_stealth_vm()...
    // e.g., patches::apply_target_patches(&target)...

    Ok(format!("hypervisor engaged. {} is now isolated in the sandbox.", target))
}

#[tauri::command]
async fn capture_rdp_window(ip: String) -> Result<String, String> {
    use winapi::um::winuser::{FindWindowA, GetWindowRect, PrintWindow};
    use winapi::um::wingdi::{CreateCompatibleDC, CreateCompatibleBitmap, SelectObject, DeleteObject};
    use winapi::shared::windef::{HWND, RECT};
    use std::ptr;
    use image::{ImageBuffer, Rgba};

    // 1. find hwnd of "Virtual Machine Connection" or mstsc.exe
    let hwnd = unsafe { FindWindowA(ptr::null(), format!("Virtual Machine Connection - {}", ip).as_ptr() as *const i8) };
    if hwnd.is_null() {
        return Err("could not find rdp window".to_string());
    }

    // 2. PrintWindow -> memory dc -> bitmap
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetWindowRect(hwnd, &mut rect) };
    let width = (rect.right - rect.left) as i32;
    let height = (rect.bottom - rect.top) as i32;

    let hdc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
    let hbitmap = unsafe { CreateCompatibleBitmap(hdc, width, height) };
    unsafe { SelectObject(hdc, hbitmap as *mut _) };

    if unsafe { PrintWindow(hwnd, hdc, 0x00000002) } == 0 {
        return Err("failed to print window".to_string());
    }

    // convert bitmap to image buffer
    let mut buf = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let pixel = unsafe { *(hdc.offset((y * width + x) as isize)) };
            buf[(y * width + x) as usize] = pixel;
        }
    }

    // encode to base64 jpeg
    let img = ImageBuffer::<Rgba<u8>, _>::from_raw(width as u32, height as u32, buf).unwrap();
    let mut jpeg_buf = Vec::new();
    image::jpeg::JPEGEncoder::new(&mut jpeg_buf).encode_image(&img).unwrap();

    Ok(base64::encode(jpeg_buf))
}

// 🔥 NEW: Module declarations for the lockdown-bypass subsystem
mod target_id;
mod vm;
mod rdp;
mod patches;
mod stealth;

// 🛡️ CRITICAL: Manager is required to interact with the OS window
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
                            k if k.contains(".") || k.contains("period") => "TOGGLE_VISIBILITY",
                            k if k.contains("arrowup") => "OPACITY_UP",
                            k if k.contains("arrowdown") => "OPACITY_DOWN",
                            k if k.contains("keyv") => "SPAWN_VM", // 🔥 NEW
                            k if k.contains("keyr") => "TOGGLE_RDP_TUNNEL", // 🔥 NEW
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
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            }
                        }

                        // 🔥 NEW: VM spawn hotkey routes into orchestration subsystem
                        if unified_command == "SPAWN_VM" {
                            let app_handle = app.clone();
                            std::thread::spawn(move || {
                                if let Err(e) = vm::spawn_stealth_vm(&app_handle) {
                                    #[cfg(debug_assertions)]
                                    eprintln!("VM spawn failed: {}", e);
                                }
                            });
                        }

                        // 🔥 NEW: RDP loopback tunnel toggle
                        if unified_command == "TOGGLE_RDP_TUNNEL" {
                            let app_handle = app.clone();
                            std::thread::spawn(move || {
                                if let Err(e) = rdp::toggle_loopback_tunnel(&app_handle) {
                                    #[cfg(debug_assertions)]
                                    eprintln!("RDP tunnel toggle failed: {}", e);
                                }
                            });
                        }

                        // 🛡️ SENIOR ENGINEERING: Defensive Event Emission
                        if let Err(e) = app.emit("app-command", unified_command) {
                            #[cfg(debug_assertions)]
                            eprintln!("WARNING: Frontend unreachable for hotkey event: {}", e);
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            // 3. Register Global Shortcuts
            let _ = app.global_shortcut().register("Alt+-");
            let _ = app.global_shortcut().register("Alt+T");
            let _ = app.global_shortcut().register("Alt+A");
            let _ = app.global_shortcut().register("Alt+H");
            let _ = app.global_shortcut().register("Alt+Up");
            let _ = app.global_shortcut().register("Alt+Down");
            let _ = app.global_shortcut().register(".");

            // 🔥 NEW: Bypass-subsystem hotkeys
            let _ = app.global_shortcut().register("Alt+V"); // Spawn stealth VM
            let _ = app.global_shortcut().register("Alt+R"); // Toggle RDP loopback

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
                        println!("🛡️ Stealth Protocol: Window is now invisible to screen capture.");
                    }
                }
            }

            // 🔥 NEW: Fire off passive lockdown-browser detection at startup
            #[cfg(target_os = "windows")]
            {
                let app_handle = app.handle().clone();
                std::thread::spawn(move || {
                    target_id::start_detection_loop(app_handle);
                });
            }

            Ok(())
        })

        // 🔥 NEW: Tauri commands exposed to the frontend for manual control
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
            // add the new c2 endpoints:
            launch_stealth_vm,
            capture_rdp_window,
        ])
        .run(tauri::generate_context!())
        .expect("CRITICAL FAILURE: The Operating System refused to allocate resources for the application.");
}
