use serde::Serialize;

#[derive(Serialize, Clone, Default)]
pub struct Captured {
    pub text: String,
    /// selection | field | clipboard | none
    pub source: &'static str,
    pub app: String,
    /// Set only when reading was refused for privacy; then no clipboard fallback either.
    pub error: Option<String>,
}

#[cfg(windows)]
pub use win::{capture, cursor_pos};

#[cfg(not(windows))]
// ponytail: macOS stub — clipboard fallback only, AXUIElement goes here later
pub fn capture(_blocklist: &[String]) -> Captured {
    Captured { source: "none", ..Default::default() }
}
#[cfg(not(windows))]
pub fn cursor_pos() -> (i32, i32) {
    (200, 200)
}

#[cfg(windows)]
mod win {
    use super::Captured;
    use uiautomation::patterns::{UITextPattern, UIValuePattern};
    use uiautomation::UIAutomation;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, POINT};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId};

    pub fn cursor_pos() -> (i32, i32) {
        let mut p = POINT::default();
        let _ = unsafe { GetCursorPos(&mut p) };
        (p.x, p.y)
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

    pub fn capture(blocklist: &[String]) -> Captured {
        let app = foreground_app();
        if blocklist.iter().any(|b| b.eq_ignore_ascii_case(&app)) {
            return Captured { app: app.clone(), error: Some(format!("{app} в чёрном списке, текст не читаю")), ..Default::default() };
        }
        let mut c = Captured { app, source: "none", ..Default::default() };
        let Ok(auto) = UIAutomation::new() else { return c };
        let Ok(el) = auto.get_focused_element() else { return c };
        if el.is_password().unwrap_or(false) {
            c.error = Some("Поле пароля, текст не читаю".into());
            return c;
        }
        if let Ok(tp) = el.get_pattern::<UITextPattern>() {
            let sel = tp
                .get_selection()
                .unwrap_or_default()
                .iter()
                .filter_map(|r| r.get_text(-1).ok())
                .collect::<Vec<_>>()
                .join("\n");
            if !sel.trim().is_empty() {
                c.text = sel;
                c.source = "selection";
                return c;
            }
            if let Ok(t) = tp.get_document_range().and_then(|r| r.get_text(-1)) {
                if !t.trim().is_empty() {
                    c.text = t;
                    c.source = "field";
                    return c;
                }
            }
        }
        if let Ok(v) = el.get_pattern::<UIValuePattern>().and_then(|p| p.get_value()) {
            if !v.trim().is_empty() {
                c.text = v;
                c.source = "field";
            }
        }
        c
    }
}
