// src-tauri/src/rdp/input.rs
// Input forwarding into the RDP session.
// Handles keystroke and mouse injection into the mstsc client window
// with per-target anti-detection configuration.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardState, MapVirtualKeyW, SendInput, KEYBD_EVENT_FLAGS,
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEEVENTF_HWHEEL, VIRTUAL_KEY,MOUSE_EVENT_FLAGS, MAPVK_VK_TO_VSC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClientRect, GetForegroundWindow, GetWindowRect,
    SetForegroundWindow,
};

use windows::Win32::Graphics::Gdi::ScreenToClient;
use rand::Rng;

use crate::target_id::TargetProfile;
use crate::rdp::client::RDPClientHandle;

/// Input mode determines how aggressively we try to make synthetic input
/// indistinguishable from physical input at the OS level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    /// Standard SendInput with scancode. Most compatible, detectable by
    /// low-level hooks checking LLKHF_INJECTED flag.
    Standard,
    /// Scancode-based with timing jitter. Slower but harder to distinguish
    /// from real typing when the target checks input timing patterns.
    Humanized,
    /// Scancode + humanized timing + key rollback verification.
    /// For targets with active input forensics (ProctorU, Honorlock).
    Forensic,
    /// Input disabled entirely — some profiles detect all synthetic input
    /// regardless of method. Operator uses the VM manually.
    Disabled,
}

/// Per-profile input configuration.
#[derive(Debug, Clone)]
pub struct InputConfig {
    pub mode: InputMode,
    /// Base delay between keystrokes in milliseconds.
    pub keystroke_delay_ms: u32,
    /// Random jitter range added to keystroke delay (0..=jitter).
    pub keystroke_jitter_ms: u32,
    /// Occasionally pause longer (thinking time) — probability per key.
    pub hesitation_probability: f32,
    /// Hesitation duration range in ms.
    pub hesitation_ms: (u32, u32),
    /// Mouse path interpolation steps (more steps = smoother).
    pub mouse_interpolation_steps: u32,
    /// Mouse curve deviation in pixels (Bezier control point offset).
    pub mouse_curve_deviation: u32,
    /// Delay between mouse move steps in ms.
    pub mouse_step_delay_ms: u32,
    /// Verify key state after sending (readback check).
    pub verify_key_state: bool,
    /// Modifier key hold duration in ms (shift, ctrl, etc).
    pub modifier_hold_ms: u32,
    /// Typing burst behavior — humans type in bursts then pause.
    pub burst_length_range: (u32, u32),
    pub burst_pause_range: (u32, u32),
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            mode: InputMode::Standard,
            keystroke_delay_ms: 45,
            keystroke_jitter_ms: 35,
            hesitation_probability: 0.03,
            hesitation_ms: (200, 800),
            mouse_interpolation_steps: 12,
            mouse_curve_deviation: 8,
            mouse_step_delay_ms: 8,
            verify_key_state: false,
            modifier_hold_ms: 60,
            burst_length_range: (4, 9),
            burst_pause_range: (300, 1500),
        }
    }
}

impl InputConfig {
    pub fn from_profile(profile: &TargetProfile) -> Self {
        let mut config = Self::default();

        match profile.target_id.as_str() {
            "respondus" => {
                // Respondus checks LLKHF_INJECTED on WH_KEYBOARD_LL hooks.
                // Use scancode-based input with humanized timing.
                config.mode = InputMode::Humanized;
                config.verify_key_state = true;
            }
            "proctoru" => {
                // ProctorU runs process enumeration and may deploy keyloggers.
                // Use forensic mode with rollback verification.
                config.mode = InputMode::Forensic;
                config.verify_key_state = true;
                config.keystroke_delay_ms = 65;
                config.keystroke_jitter_ms = 50;
                config.hesitation_probability = 0.05;
            }
            "honorlock" => {
                // Honorlock uses browser extension-level monitoring.
                // Less aggressive on OS-level input detection.
                config.mode = InputMode::Humanized;
            }
            "examity" => {
                // Examity is primarily webcam-based, moderate input checking.
                config.mode = InputMode::Humanized;
            }
            "proctorio" => {
                // Proctorio checks WebRTC and adapter info more than input.
                config.mode = InputMode::Standard;
            }
            _ => {
                // Generic/unidentified — use humanized as safe default.
                config.mode = InputMode::Humanized;
            }
        }

        config
    }
}

/// Manages input forwarding into the RDP client window.
pub struct InputForwarder {
    /// Handle to the mstsc client window.
    client_hwnd: Option<HWND>,
    /// Client window client rect (guest viewport on host screen).
    client_rect: RECT,
    /// Guest screen resolution (e.g., 1920x1080).
    guest_resolution: (u32, u32),
    /// Active input configuration.
    config: InputConfig,
    /// Random state for timing jitter.
    // rng: rand::thread_rng(),  <- commented this
    /// Track burst state for natural typing rhythm.
    burst_remaining: u32,
    /// Last input timestamp.
    last_input: Option<Instant>,
    /// Currently held modifier keys.
    held_modifiers: Vec<VIRTUAL_KEY>,
    /// Pending verification keys.
    pending_verify: HashMap<VIRTUAL_KEY, bool>,
    rng: rand::rngs::ThreadRng,
}

impl InputForwarder {
    pub fn new(client: &RDPClientHandle, config: InputConfig) -> Result<Self, InputError> {
        let hwnd = client.get_window_handle()
            .ok_or(InputError::ClientNotRunning)?;

        let client_rect = unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect).map_err(|_| InputError::WindowQueryFailed)?;
            rect
        };

        let guest_res = client.get_guest_resolution();
        let rng = rand::thread_rng();

        Ok(Self {
            rng,
            client_hwnd: Some(hwnd),
            client_rect,
            guest_resolution: guest_res,
            config,
            burst_remaining: 0,
            last_input: None,
            held_modifiers: Vec::new(),
            pending_verify: HashMap::new(),
        })
    }

    /// Bring the RDP client window to the foreground and ensure it
    /// receives input.
    pub fn focus_client(&mut self) -> Result<(), InputError> {
        let hwnd = self.client_hwnd.ok_or(InputError::ClientNotRunning)?;

        unsafe {
            if !SetForegroundWindow(hwnd).as_bool() {
    return Err(InputError::FocusFailed);
}
        }

        Ok(())
    }

    /// Translate guest coordinates to host screen coordinates.
    /// Guest (x, y) in guest resolution → host screen pixels where mstsc
    /// renders the guest viewport.
    fn guest_to_host(&self, guest_x: u32, guest_y: u32) -> Result<(i32, i32), InputError> {
        let hwnd = self.client_hwnd.ok_or(InputError::ClientNotRunning)?;

        let client_width = (self.client_rect.right - self.client_rect.left).max(1);
        let client_height = (self.client_rect.bottom - self.client_rect.top).max(1);

        let scale_x = client_width as f64 / self.guest_resolution.0 as f64;
        let scale_y = client_height as f64 / self.guest_resolution.1 as f64;

        let local_x = (guest_x as f64 * scale_x).round() as i32;
        let local_y = (guest_y as f64 * scale_y).round() as i32;

        // Convert client coordinates to screen coordinates.
        let mut point = POINT { x: local_x, y: local_y };
        unsafe {
            if !ScreenToClient(hwnd, &mut point).as_bool() {
                return Err(InputError::CoordinateTranslationFailed);
            }
        }

        Ok((point.x, point.y))
    }

    /// Type a string of text character by character with humanized timing.
    pub fn type_text(&mut self, text: &str) -> Result<(), InputError> {
        if self.config.mode == InputMode::Disabled {
            return Err(InputError::InputDisabled);
        }

        self.focus_client()?;

        let chars: Vec<char> = text.chars().collect();
        let mut burst_count = 0;

        for (idx, ch) in chars.iter().enumerate() {
            self.apply_typing_delay(&mut burst_count)?;
            self.press_key_for_char(*ch)?;

            // Forensic mode: verify the key registered in the guest.
            if self.config.mode == InputMode::Forensic {
                self.verify_last_keypress()?;
            }

            self.last_input = Some(Instant::now());
        }

        Ok(())
    }

    /// Type text at a specific location in the guest (click first, then type).
    pub fn type_text_at(&mut self, guest_x: u32, guest_y: u32, text: &str) -> Result<(), InputError> {
        self.click(guest_x, guest_y)?;
        self.human_pause(Duration::from_millis(150))?;
        self.type_text(text)?;
        Ok(())
    }

    /// Send a single key press (down + up).
    pub fn press_key(&mut self, vk: VIRTUAL_KEY) -> Result<(), InputError> {
        if self.config.mode == InputMode::Disabled {
            return Err(InputError::InputDisabled);
        }

        self.focus_client()?;
        self.send_key_event(vk, false)?;
        self.human_pause(Duration::from_millis(self.config.modifier_hold_ms as u64))?;
        self.send_key_event(vk, true)?;

        if self.config.verify_key_state {
            self.verify_key_release(vk)?;
        }

        Ok(())
    }

    /// Send a key combination (e.g., Ctrl+C).
    pub fn press_key_combo(&mut self, modifiers: &[VIRTUAL_KEY], key: VIRTUAL_KEY) -> Result<(), InputError> {
        if self.config.mode == InputMode::Disabled {
            return Err(InputError::InputDisabled);
        }

        self.focus_client()?;

        // Press modifiers with slight stagger.
        for &m in modifiers {
            self.send_key_event(m, false)?;
            self.human_pause(Duration::from_millis(20))?;
            self.held_modifiers.push(m);
        }

        // Press main key.
        self.send_key_event(key, false)?;
        self.human_pause(Duration::from_millis(self.config.modifier_hold_ms as u64))?;
        self.send_key_event(key, true)?;

        // Release modifiers in reverse order.
        for &m in modifiers.iter().rev() {
            self.send_key_event(m, true)?;
            self.human_pause(Duration::from_millis(15))?;
            self.held_modifiers.pop();
        }

        Ok(())
    }

/// Move mouse to guest coordinates with humanized path.
    pub fn move_mouse(&mut self, guest_x: u32, guest_y: u32) -> Result<(), InputError> {
        if self.config.mode == InputMode::Disabled {
            return Err(InputError::InputDisabled);
        }

        let (host_x, host_y) = self.guest_to_host(guest_x, guest_y)?;

        // Get current cursor position for path calculation.
        let current = self.get_cursor_position()?;

        if self.config.mode == InputMode::Standard {
            // Direct move. Use the full path and cast coordinates to i32.
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::SetCursorPos(host_x as i32, host_y as i32)
                    .map_err(|_| InputError::CursorMoveFailed)?;
            }
            return Ok(());
        }

        // Humanized: interpolate a curved path.
        let steps = self.config.mouse_interpolation_steps;
        let deviation = self.config.mouse_curve_deviation;

        let start_x = current.0 as f64;
        let start_y = current.1 as f64;
        let end_x = host_x as f64;
        let end_y = host_y as f64;

        // Quadratic Bezier with random control point deviation.
        let ctrl_x = (start_x + end_x) / 2.0
            + self.rng.gen_range(-(deviation as f64)..=deviation as f64);
        let ctrl_y = (start_y + end_y) / 2.0
            + self.rng.gen_range(-(deviation as f64)..=deviation as f64);

        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let one_minus_t = 1.0 - t;

            let x = one_minus_t * one_minus_t * start_x
                + 2.0 * one_minus_t * t * ctrl_x
                + t * t * end_x;
            let y = one_minus_t * one_minus_t * start_y
                + 2.0 * one_minus_t * t * ctrl_y
                + t * t * end_y;

            // Interpolated move. Use the full path here as well.
            unsafe {
                windows::Win32::UI::Input::KeyboardAndMouse::SetCursorPos(x.round() as i32, y.round() as i32)
                    .map_err(|_| InputError::CursorMoveFailed)?;
            }

            self.human_pause(Duration::from_millis(self.config.mouse_step_delay_ms as u64))?;
        }

        Ok(())
    }

    /// Click at guest coordinates.
    pub fn click(&mut self, guest_x: u32, guest_y: u32) -> Result<(), InputError> {
        self.move_mouse(guest_x, guest_y)?;
        self.human_pause(Duration::from_millis(30))?;
        self.send_mouse_button(MOUSEEVENTF_LEFTDOWN)?;
        self.human_pause(Duration::from_millis(45))?;
               self.send_mouse_button(MOUSEEVENTF_LEFTUP)?;
        Ok(())
    }

    /// Release all currently held modifiers (recovery / cleanup).
    pub fn release_all_modifiers(&mut self) -> Result<(), InputError> {
        while let Some(vk) = self.held_modifiers.pop() {
            self.send_key_event(vk, true)?;
            self.human_pause(Duration::from_millis(15))?;
        }
        Ok(())
    }

    /// Middle-click at guest coordinates.
    pub fn middle_click(&mut self, guest_x: u32, guest_y: u32) -> Result<(), InputError> {
        self.move_mouse(guest_x, guest_y)?;
        self.human_pause(Duration::from_millis(30))?;
        self.send_mouse_button(MOUSEEVENTF_MIDDLEDOWN)?;
        self.human_pause(Duration::from_millis(45))?;
        self.send_mouse_button(MOUSEEVENTF_MIDDLEUP)?;
        Ok(())
    }

    /// Horizontal scroll (positive = right, negative = left).
    pub fn hscroll(&mut self, direction: i32, amount: i32) -> Result<(), InputError> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: windows::Win32::UI::Input::KeyboardAndMouse::MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: (amount.abs() as u32) * 120,
                    dwFlags: MOUSEEVENTF_HWHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        let inputs = [input];
        let result = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if result == 0 {
            return Err(InputError::SendInputFailed);
        }

        self.human_pause(Duration::from_millis(80))?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Private helpers
    // ------------------------------------------------------------------

    /// Apply a humanized delay between keystrokes, including burst
    /// rhythm and occasional hesitation pauses.
    fn apply_typing_delay(&mut self, burst_count: &mut u32) -> Result<(), InputError> {
        // Burst logic: when the current burst is exhausted, take a longer
        // pause before starting the next burst.
        if self.burst_remaining == 0 {
            let (min_burst, max_burst) = self.config.burst_length_range;
            self.burst_remaining = self.rng.gen_range(min_burst..=max_burst);

            let (min_pause, max_pause) = self.config.burst_pause_range;
            let pause = self.rng.gen_range(min_pause..=max_pause);
            self.human_pause(Duration::from_millis(pause as u64))?;
        }

        self.burst_remaining = self.burst_remaining.saturating_sub(1);
        *burst_count += 1;

        // Base delay + jitter.
        let base = self.config.keystroke_delay_ms;
        let jitter = self.config.keystroke_jitter_ms;
        let delay = if jitter > 0 {
            self.rng.gen_range(base..=base + jitter)
        } else {
            base
        };

        self.human_pause(Duration::from_millis(delay as u64))?;

        // Occasional hesitation ("thinking" pause).
        if self.rng.gen::<f32>() < self.config.hesitation_probability {
            let (min_h, max_h) = self.config.hesitation_ms;
            let hesitation = self.rng.gen_range(min_h..=max_h);
            self.human_pause(Duration::from_millis(hesitation as u64))?;
        }

        Ok(())
    }

    /// Sleep with small random variance so fixed delays don't produce
    /// perfectly periodic timing signatures.
    fn human_pause(&self, duration: Duration) -> Result<(), InputError> {
        if self.config.mode == InputMode::Standard {
            std::thread::sleep(duration);
            return Ok(());
        }

        // Add up to 12% variance to the requested duration.
        let variance_ms = (duration.as_millis() as f64 * 0.12).ceil() as u64;
        let extra = if variance_ms > 0 {
            self.rng.gen_range(0..=variance_ms)
        } else {
            0
        };

        std::thread::sleep(duration + Duration::from_millis(extra));
        Ok(())
    }

    /// Send a single key event for a character, using Unicode injection
    /// for characters without direct VK mappings and scancode injection
    /// otherwise.
    fn press_key_for_char(&mut self, ch: char) -> Result<(), InputError> {
        // Uppercase letters and shifted symbols need Shift held.
        let needs_shift = ch.is_ascii_uppercase()
            || matches!(ch,
                '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')'
                | '_' | '+' | '{' | '}' | '|' | ':' | '"' | '<' | '>' | '?'
            );

        if needs_shift {
            let shift = VIRTUAL_KEY(0x10); // VK_SHIFT
            self.send_key_event(shift, false)?;
            self.human_pause(Duration::from_millis(18))?;
            self.send_unicode_char(ch)?;
            self.human_pause(Duration::from_millis(18))?;
            self.send_key_event(shift, true)?;
        } else {
            self.send_unicode_char(ch)?;
        }

        Ok(())
    }

    /// Send a character via KEYEVENTF_UNICODE. This bypasses keyboard
    /// layout entirely, which is necessary for arbitrary text.
    fn send_unicode_char(&mut self, ch: char) -> Result<(), InputError> {
        let mut code_units = [0u16; 2];
        let encoded: Vec<u16> = ch.encode_utf16(&mut code_units).iter().copied().collect();

        for &unit in &encoded {
            // Key down.
            let down = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };

            // Key up.
            let up = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };

            let inputs = [down];
            let result = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
            if result == 0 {
                return Err(InputError::SendInputFailed);
            }

            // Small hold time between down and up.
            let hold = self.rng.gen_range(25..=55);
            std::thread::sleep(Duration::from_millis(hold));

            let inputs = [up];
            let result = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
            if result == 0 {
                return Err(InputError::SendInputFailed);
            }
        }

        Ok(())
    }

    /// Send a raw key event using scan codes (more realistic than
    /// VK-based injection; matches what a physical keyboard produces).
    fn send_key_event(&mut self, vk: VIRTUAL_KEY, key_up: bool) -> Result<(), InputError> {
        let scan_code = unsafe {
            MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC)
        };

        if scan_code == 0 {
            // Fall back to VK-based injection if no scancode exists.
            let flags = if key_up { KEYEVENTF_KEYUP } else { windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0) };
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };

            let inputs = [input];
            let result = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
            if result == 0 {
                return Err(InputError::SendInputFailed);
            }
            return Ok(());
        }

        // Extended keys (arrows, insert/delete block, numpad divider)
        // require the EXTENDEDKEY flag to behave correctly.
        let is_extended = matches!(vk.0,
            0x21 | 0x22 | 0x23 | 0x24 | 0x25 | 0x26 | 0x27 | 0x28 | // PgUp/PgDn/End/Home/arrows
            0x2D | 0x2E |                                             // Insert/Delete
            0x5B | 0x5C |                                             // Win keys
            0x6F                                                      // Numpad divide
        );

        let mut flags = KEYEVENTF_SCANCODE;
        if is_extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if key_up {
            flags |= KEYEVENTF_KEYUP;
        }

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan_code as u16,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        let inputs = [input];
        let result = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if result == 0 {
            return Err(InputError::SendInputFailed);
        }

        Ok(())
    }

    /// Send a mouse button event at the current cursor position.
    fn send_mouse_button(&mut self, flags: MOUSE_EVENT_FLAGS) -> Result<(), InputError> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: windows::Win32::UI::Input::KeyboardAndMouse::MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        let inputs = [input];
        let result = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if result == 0 {
            return Err(InputError::SendInputFailed);
        }

        Ok(())
    }

    /// Get the current cursor position in screen coordinates.
    fn get_cursor_position(&self) -> Result<(i32, i32), InputError> {
        let mut point = POINT::default();
        unsafe {
            let result = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point);
            result.map_err(|_| InputError::CursorQueryFailed)?;
        }
        Ok((point.x, point.y))
    }

    /// Forensic mode: verify that the last keypress registered in the
    /// OS keyboard state before proceeding.
    fn verify_last_keypress(&mut self) -> Result<(), InputError> {
        // Brief settle delay before reading state.
        std::thread::sleep(Duration::from_millis(5));

        let state_ok = unsafe {
            let mut keyboard_state = [0u8; 256];
            let ok = GetKeyboardState(&mut keyboard_state);
            if ok.is_err() {
                return Err(InputError::VerificationFailed);
            }
            keyboard_state
        };

        // If any tracked key is stuck down unexpectedly, roll it back.
        for (vk, expected_down) in self.pending_verify.clone().iter() {
            let vk_index = vk.0 as usize;
            if vk_index < 256 {
                let is_down = (state_ok[vk_index] & 0x80) != 0;
                if is_down && !expected_down {
                    // Key stuck — send an explicit key-up to recover.
                    self.send_key_event(*vk, true)?;
                }
            }
        }

        self.pending_verify.clear();
        Ok(())
    }

    /// Verify a specific key is fully released after a press.
    fn verify_key_release(&mut self, vk: VIRTUAL_KEY) -> Result<(), InputError> {
        std::thread::sleep(Duration::from_millis(5));

        unsafe {
            let state = GetAsyncKeyState(vk.0 as i32);
            // High bit indicates currently pressed.
            if (state as u32 & 0x8000) != 0 {
                // Stuck key — force release.
                self.send_key_event(vk, true)?;
                std::thread::sleep(Duration::from_millis(5));
                let state = GetAsyncKeyState(vk.0 as i32);
                if (state as u32 & 0x8000) != 0 {
                    return Err(InputError::VerificationFailed);
                }
            }
        }

        Ok(())
    }

    /// Update the guest resolution if the RDP session was resized.
    pub fn update_guest_resolution(&mut self, resolution: (u32, u32)) {
        self.guest_resolution = resolution;
    }

    /// Re-acquire the client window rect (e.g., after window resize or move).
    pub fn refresh_client_rect(&mut self) -> Result<(), InputError> {
        let hwnd = self.client_hwnd.ok_or(InputError::ClientNotRunning)?;

        self.client_rect = unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect).map_err(|_| InputError::WindowQueryFailed)?;
            rect
        };

        Ok(())
    }

    /// Check whether the RDP client window is still the foreground window.
    pub fn is_client_focused(&self) -> bool {
        let hwnd = match self.client_hwnd {
            Some(h) => h,
            None => return false,
        };

        unsafe { GetForegroundWindow() == hwnd }
    }

    /// Attempt to re-locate the mstsc client window by class name if the
    /// stored handle has become stale.
    pub fn reacquire_client(&mut self, class_name: &str) -> Result<(), InputError> {
        let wide: Vec<u16> = class_name.encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = unsafe {
            FindWindowW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::core::PCWSTR::null(),
            )
        };

        let hwnd = hwnd.map_err(|e| InputError::WindowNotFound(e.to_string()))?;
        if hwnd.0.is_null() {
        return Err(InputError::WindowNotFound("null handle".into()));
        }
    self.client_hwnd = Some(hwnd);
        self.refresh_client_rect()?;
        Ok(())
    }
}

/// Errors that can occur during input forwarding.
#[derive(Debug)]
pub enum InputError {
    WindowNotFound(String),
    ClientNotRunning,
    WindowQueryFailed,
    FocusFailed,
    CoordinateTranslationFailed,
    CursorMoveFailed,
    CursorQueryFailed,
    SendInputFailed,
    InputDisabled,
    VerificationFailed,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::ClientNotRunning => write!(f, "RDP client is not running"),
            InputError::WindowQueryFailed => write!(f, "Failed to query the client window"),
            InputError::FocusFailed => write!(f, "Failed to bring client window to foreground"),
            InputError::CoordinateTranslationFailed => {
                write!(f, "Failed to translate guest coordinates to host screen")
            }
            InputError::CursorMoveFailed => write!(f, "Failed to move the cursor"),
            InputError::CursorQueryFailed => write!(f, "Failed to query cursor position"),
            InputError::SendInputFailed => write!(f, "SendInput call failed"),
            InputError::InputDisabled => write!(f, "Input is disabled for this target profile"),
            InputError::VerificationFailed => {
                write!(f, "Key state verification failed after injection")
            }
        }
    }
}

impl std::error::Error for InputError {}