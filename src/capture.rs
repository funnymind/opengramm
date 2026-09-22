use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Captured {
    pub text: String,
    /// selection | field | line | clipboard | none
    pub source: String,
    pub app: String,
    /// Set only when reading was refused for privacy; then no clipboard fallback either.
    pub error: Option<String>,
    /// Caret (or field) rect in physical screen pixels: x, y, w, h
    pub caret: Option<[i32; 4]>,
}

/// Terminal lines come with a prompt in front; keep only what the user typed.
/// ponytail: heuristic for PowerShell, cmd, bash and TUI input boxes; extend as new prompts show up
pub fn strip_prompt(line: &str) -> &str {
    let s = line.trim_matches(|c: char| c.is_whitespace() || "│┃|╭╮╰╯─".contains(c));
    if s.starts_with("PS ") {
        if let Some(i) = s.find("> ") {
            return s[i + 2..].trim();
        }
    }
    let b = s.as_bytes();
    if b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
        if let Some(i) = s.find('>') {
            return s[i + 1..].trim();
        }
    }
    if let Some(rest) = s.strip_prefix(['>', '❯', '›', '$', '#', '%']) {
        return rest.trim();
    }
    if let Some(i) = s.find("$ ") {
        if !s[..i].contains(' ') {
            return s[i + 2..].trim();
        }
    }
    s
}

#[test]
fn strips_prompts() {
    assert_eq!(strip_prompt("PS C:\\Users\\b> привет мир"), "привет мир");
    assert_eq!(strip_prompt("C:\\Users\\b>привет"), "привет");
    assert_eq!(strip_prompt("│ > Првт как деал      │"), "Првт как деал");
    assert_eq!(strip_prompt("user@host:~/x$ echo hi"), "echo hi");
    assert_eq!(strip_prompt("обычный текст, без $ промпта"), "обычный текст, без $ промпта");
}

#[cfg(windows)]
pub use win::{capture, cursor_pos, hide_raw, show_no_activate};

#[cfg(not(windows))]
// ponytail: macOS stub — clipboard fallback only, AXUIElement goes here later
pub fn capture(_blocklist: &[String]) -> Captured {
    Captured { source: "none".into(), ..Default::default() }
}
#[cfg(not(windows))]
pub fn cursor_pos() -> (i32, i32) {
    (200, 200)
}
#[cfg(not(windows))]
pub fn show_no_activate(win: &tauri::WebviewWindow) {
    let _ = win.show();
}
#[cfg(not(windows))]
pub fn hide_raw(win: &tauri::WebviewWindow) {
    let _ = win.hide();
}

#[cfg(windows)]
mod win {
    use super::{strip_prompt, Captured};
    use std::ffi::c_void;
    use uiautomation::patterns::{UITextPattern, UIValuePattern};
    use uiautomation::types::TextUnit;
    use uiautomation::{UIAutomation, UIElement};
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, POINT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetUBound, SafeArrayUnaccessData};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, ShowWindow, GUITHREADINFO,
        SW_HIDE, SW_SHOWNOACTIVATE,
    };

    pub fn cursor_pos() -> (i32, i32) {
        let mut p = POINT::default();
        let _ = unsafe { GetCursorPos(&mut p) };
        (p.x, p.y)
    }

    /// Show without stealing focus from the field the user is typing in
    pub fn show_no_activate(win: &tauri::WebviewWindow) {
        if let Ok(hwnd) = win.hwnd() {
            let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        }
    }

    /// Pair of `show_no_activate`: tauri's hide() is a no-op here, it never saw the window shown
    pub fn hide_raw(win: &tauri::WebviewWindow) {
        if let Ok(hwnd) = win.hwnd() {
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
    }

    fn foreground_app() -> String {
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return String::new();
            };
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
            let _ = CloseHandle(h);
            if ok.is_err() {
                return String::new();
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            path.rsplit('\\').next().unwrap_or_default().to_string()
        }
    }

    /// Last rectangle of a text range (the caret sits at the end of a selection)
    fn rect_of(range: &uiautomation::patterns::UITextRange) -> Option<[i32; 4]> {
        unsafe {
            let psa = range.as_ref().GetBoundingRectangles().ok()?;
            if psa.is_null() {
                return None;
            }
            let mut out = None;
            let ub = SafeArrayGetUBound(psa, 1).unwrap_or(-1);
            let mut p: *mut c_void = std::ptr::null_mut();
            if ub >= 3 && SafeArrayAccessData(psa, &mut p).is_ok() {
                let d = std::slice::from_raw_parts(p as *const f64, ub as usize + 1);
                let n = (d.len() / 4 - 1) * 4;
                out = Some([d[n] as i32, d[n + 1] as i32, d[n + 2] as i32, d[n + 3] as i32]);
                let _ = SafeArrayUnaccessData(psa);
            }
            let _ = SafeArrayDestroy(psa);
            out
        }
    }

    fn caret_range(tp: &UITextPattern) -> Option<uiautomation::patterns::UITextRange> {
        tp.get_caret_range()
            .ok()
            .map(|(_, r)| r)
            .or_else(|| tp.get_selection().ok()?.into_iter().next())
    }

    fn uia_caret(tp: &UITextPattern) -> Option<[i32; 4]> {
        let range = caret_range(tp)?;
        rect_of(&range).or_else(|| {
            // degenerate range has no rect in many apps: widen to one character
            range.expand_to_enclosing_unit(TextUnit::Character).ok()?;
            rect_of(&range)
        })
    }

    /// Classic Win32 caret, for apps without a usable TextPattern
    fn gui_caret() -> Option<[i32; 4]> {
        unsafe {
            let tid = GetWindowThreadProcessId(GetForegroundWindow(), None);
            let mut gi = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
            GetGUIThreadInfo(tid, &mut gi).ok()?;
            if gi.hwndCaret.is_invalid() {
                return None;
            }
            let r = gi.rcCaret;
            let mut p = POINT { x: r.left, y: r.top };
            let _ = ClientToScreen(gi.hwndCaret, &mut p);
            Some([p.x, p.y, r.right - r.left, r.bottom - r.top])
        }
    }

    fn read_text(el: &UIElement, tp: Option<&UITextPattern>, terminal: bool) -> Option<(String, &'static str)> {
        if let Some(tp) = tp {
            let sel = tp
                .get_selection()
                .unwrap_or_default()
                .iter()
                .filter_map(|r| r.get_text(-1).ok())
                .collect::<Vec<_>>()
                .join("\n");
            if !sel.trim().is_empty() {
                return Some((sel, "selection"));
            }
            if terminal {
                // whole scrollback is useless (and may be huge): take the line under the cursor
                let range = caret_range(tp)?;
                range.expand_to_enclosing_unit(TextUnit::Line).ok()?;
                let line = strip_prompt(&range.get_text(-1).ok()?).to_string();
                return (!line.is_empty()).then_some((line, "line"));
            }
            if let Ok(t) = tp.get_document_range().and_then(|r| r.get_text(-1)) {
                if !t.trim().is_empty() {
                    return Some((t, "field"));
                }
            }
        }
        let v = el.get_pattern::<UIValuePattern>().and_then(|p| p.get_value()).ok()?;
        (!v.trim().is_empty() && !terminal).then_some((v, "field"))
    }

    pub fn capture(blocklist: &[String]) -> Captured {
        let app = foreground_app();
        if blocklist.iter().any(|b| b.eq_ignore_ascii_case(&app)) {
            return Captured { error: Some(format!("{app} в чёрном списке, текст не читаю")), app, ..Default::default() };
        }
        let mut c = Captured { app, source: "none".into(), ..Default::default() };
        let el = UIAutomation::new().and_then(|a| a.get_focused_element());
        let Ok(el) = el else {
            c.caret = gui_caret();
            return c;
        };
        if el.is_password().unwrap_or(false) {
            c.error = Some("Поле пароля, текст не читаю".into());
            return c;
        }
        let terminal = matches!(el.get_classname().as_deref(), Ok("TermControl" | "ConsoleWindowClass"))
            || c.app.eq_ignore_ascii_case("WindowsTerminal.exe");
        let tp = el.get_pattern::<UITextPattern>().ok();
        if let Some((text, source)) = read_text(&el, tp.as_ref(), terminal) {
            c.text = text;
            c.source = source.into();
        }
        c.caret = tp
            .as_ref()
            .and_then(uia_caret)
            .or_else(gui_caret)
            .or_else(|| el.get_bounding_rectangle().ok().map(|r| [r.get_left(), r.get_top(), r.get_width(), r.get_height()]));
        c
    }
}
