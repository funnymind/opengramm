//! "User stopped typing" detector. The keyboard hook records only *when* a key was
//! pressed, never which one.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static LAST_KEY_MS: AtomicU64 = AtomicU64::new(0);
static DIRTY: AtomicBool = AtomicBool::new(false);

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// True once per typing burst, after `delay_ms` of silence
pub fn take_if_idle(delay_ms: u64) -> bool {
    DIRTY.load(Ordering::Relaxed)
        && now_ms().saturating_sub(LAST_KEY_MS.load(Ordering::Relaxed)) >= delay_ms
        && DIRTY.swap(false, Ordering::Relaxed)
}

/// Put the burst back, e.g. when a check was skipped by the rate limit
pub fn mark_dirty() {
    DIRTY.store(true, Ordering::Relaxed);
}

#[cfg(windows)]
pub fn start_hook() {
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU,
        VK_RSHIFT, VK_RWIN, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN,
    };

    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 && wparam.0 as u32 == WM_KEYDOWN {
            let vk = (*(lparam.0 as *const KBDLLHOOKSTRUCT)).vkCode as u16;
            let modifier = [VK_SHIFT, VK_LSHIFT, VK_RSHIFT, VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN]
                .iter()
                .any(|k| k.0 == vk);
            // shortcuts (Ctrl+…, Win+…) are not typing; Alt+… arrives as WM_SYSKEYDOWN
            let chord = [VK_CONTROL, VK_LWIN, VK_RWIN].iter().any(|k| GetAsyncKeyState(k.0 as i32) < 0);
            if !modifier && !chord {
                LAST_KEY_MS.store(now_ms(), Ordering::Relaxed);
                DIRTY.store(true, Ordering::Relaxed);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    std::thread::spawn(|| unsafe {
        now_ms();
        let Ok(_hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), None, 0) else { return };
        // LL hooks are delivered through this thread's message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
    });
}

#[cfg(not(windows))]
// ponytail: macOS needs a CGEventTap + Accessibility permission; auto-check is off there for now
pub fn start_hook() {}
