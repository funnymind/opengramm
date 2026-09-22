#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod capture;
mod typing;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, State, WebviewWindow, WindowEvent};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const SYSTEM_PROMPT: &str = "Ты редактор текста. Перепиши текст пользователя по инструкции. \
Сохраняй язык оригинала, смысл, имена, ссылки и форматирование. \
Верни только готовый текст, без пояснений, вступлений и кавычек.";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    /// opens the full window
    hotkey: String,
    /// shows the small error badge at the caret
    check_hotkey: String,
    /// LanguageTool language code, "auto" to detect
    language: String,
    lt_url: String,
    openrouter_key: String,
    model: String,
    /// Process names (e.g. "KeePassXC.exe") whose text is never read
    blocklist: Vec<String>,
    /// Check automatically when typing pauses
    auto_check: bool,
    auto_delay_ms: u64,
    /// At most one LanguageTool request per this interval (public API: 20/min)
    auto_min_interval_ms: u64,
    /// Longer fields are checked by the paragraph at the caret
    auto_max_chars: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "ctrl+alt+g".into(),
            check_hotkey: "ctrl+alt+h".into(),
            language: "auto".into(),
            lt_url: "https://api.languagetool.org/v2/check".into(),
            openrouter_key: String::new(),
            model: "anthropic/claude-haiku-4.5".into(),
            blocklist: ["KeePass.exe", "KeePassXC.exe", "1Password.exe", "Bitwarden.exe", "mstsc.exe"]
                .map(String::from)
                .to_vec(),
            auto_check: true,
            auto_delay_ms: 1500,
            auto_min_interval_ms: 3000,
            auto_max_chars: 2000,
        }
    }
}

fn config_path(app: &AppHandle) -> PathBuf {
    app.path().app_config_dir().expect("no config dir").join("config.json")
}

fn load_config(app: &AppHandle) -> Config {
    let path = config_path(app);
    if !path.exists() {
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&Config::default()).unwrap());
    }
    let mut cfg: Config = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if cfg.openrouter_key.is_empty() {
        cfg.openrouter_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    }
    cfg
}

fn on_hotkey(app: &AppHandle, badge: bool) {
    let app = app.clone();
    // UIA initializes COM itself; keep it off the UI thread
    std::thread::spawn(move || {
        let cfg = app.state::<Config>();
        let mut c = capture::capture(&cfg.blocklist, usize::MAX);
        if c.error.is_none() && c.text.trim().is_empty() {
            c.text = app.clipboard().read_text().unwrap_or_default();
            c.source = "clipboard".into();
        }
        if badge {
            show_badge(&app, c);
        } else {
            show_popup(&app, c);
        }
    });
}

/// Put the window just under the caret (or the mouse), kept inside the monitor.
/// `reserve_w` is the width to keep free on the right, for windows that grow after showing.
fn place(win: &WebviewWindow, caret: Option<[i32; 4]>, reserve_w: u32) {
    let (mut x, mut y) = match caret {
        Some([x, y, _, h]) => (x, y + h + 4),
        None => {
            let (x, y) = capture::cursor_pos();
            (x, y + 16)
        }
    };
    if let (Ok(Some(m)), Ok(size)) = (win.monitor_from_point(x as f64, y as f64), win.outer_size()) {
        let (mp, ms) = (m.position(), m.size());
        x = x.min(mp.x + ms.width as i32 - size.width.max(reserve_w) as i32).max(mp.x);
        y = y.min(mp.y + ms.height as i32 - size.height as i32).max(mp.y);
    }
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

fn show_popup(app: &AppHandle, c: capture::Captured) {
    if let Some(b) = app.get_webview_window("badge") {
        capture::hide_raw(&b);
    }
    let Some(win) = app.get_webview_window("main") else { return };
    place(&win, c.caret, 0);
    let _ = win.emit("captured", c);
    let _ = win.show();
    let _ = win.set_focus();
}

/// Badge renders itself, then calls `badge_show` with its real size
fn show_badge(app: &AppHandle, c: capture::Captured) {
    let Some(win) = app.get_webview_window("badge") else { return };
    capture::hide_raw(&win);
    place(&win, c.caret, (540.0 * win.scale_factor().unwrap_or(1.0)) as u32);
    let _ = win.emit("captured", c);
}

#[tauri::command]
fn badge_show(window: WebviewWindow, w: f64, h: f64) {
    let _ = window.set_size(LogicalSize::new(w, h));
    capture::show_no_activate(&window);
}

#[tauri::command]
fn badge_hide(window: WebviewWindow) {
    capture::hide_raw(&window);
}

#[tauri::command]
fn open_full(app: AppHandle, c: capture::Captured) {
    show_popup(&app, c);
}

fn err(e: impl ToString) -> String {
    e.to_string()
}

/// Runs on its own thread: one LT request per typing pause, only if the text changed
fn auto_loop(app: AppHandle) {
    let cfg = app.state::<Config>().inner().clone();
    let own_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let min_interval = Duration::from_millis(cfg.auto_min_interval_ms);
    let mut last_check: Option<Instant> = None;
    let mut last_text = String::new();
    typing::start_hook();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if !typing::take_if_idle(cfg.auto_delay_ms) {
            continue;
        }
        if last_check.is_some_and(|t| t.elapsed() < min_interval) {
            typing::mark_dirty(); // retry on a later tick
            continue;
        }
        let c = capture::capture(&cfg.blocklist, cfg.auto_max_chars);
        // no clipboard fallback and no error badges here: stay silent unless there is something to fix
        if c.error.is_some() || c.text.trim().is_empty() || c.app.eq_ignore_ascii_case(&own_exe) {
            continue;
        }
        if c.text == last_text || c.text.chars().count() > cfg.auto_max_chars {
            continue;
        }
        last_check = Some(Instant::now());
        last_text = c.text.clone();
        let Ok(r) = tauri::async_runtime::block_on(lt(&cfg, &c.text)) else { continue };
        let Some(win) = app.get_webview_window("badge") else { continue };
        if r["matches"].as_array().is_some_and(|m| !m.is_empty()) {
            place(&win, c.caret, (540.0 * win.scale_factor().unwrap_or(1.0)) as u32);
        }
        let _ = win.emit("checked", json!({ "c": c, "r": r }));
    }
}

async fn lt(cfg: &Config, text: &str) -> Result<Value, String> {
    let res = reqwest::Client::new()
        .post(&cfg.lt_url)
        .form(&[("text", text), ("language", cfg.language.as_str())])
        .send()
        .await
        .map_err(err)?;
    if !res.status().is_success() {
        return Err(format!("LanguageTool {}: {}", res.status(), res.text().await.unwrap_or_default()));
    }
    res.json().await.map_err(err)
}

#[tauri::command]
async fn lt_check(text: String, cfg: State<'_, Config>) -> Result<Value, String> {
    lt(&cfg, &text).await
}

#[tauri::command]
async fn rewrite(text: String, instruction: String, cfg: State<'_, Config>) -> Result<String, String> {
    if cfg.openrouter_key.is_empty() {
        return Err("Нет ключа OpenRouter: укажите openrouter_key в config.json (трей → Настройки) или OPENROUTER_API_KEY".into());
    }
    let body = json!({
        "model": cfg.model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": format!("Инструкция: {instruction}\n\nТекст:\n{text}")}
        ]
    });
    let v: Value = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&cfg.openrouter_key)
        .json(&body)
        .send()
        .await
        .map_err(err)?
        .json()
        .await
        .map_err(err)?;
    v["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| format!("OpenRouter: {}", v["error"]["message"].as_str().unwrap_or(&v.to_string())))
}

#[tauri::command]
fn source_info(cfg: State<'_, Config>) -> Value {
    json!({ "model": cfg.model, "lt": cfg.lt_url, "hotkey": cfg.hotkey })
}

fn open_config(app: &AppHandle) {
    let path = config_path(app);
    #[cfg(windows)]
    let _ = std::process::Command::new("notepad").arg(path).spawn();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("open").arg("-t").arg(path).spawn();
}

fn main() {
    tauri::Builder::default()
        // must be first: a second launch exits right away
        .plugin(tauri_plugin_single_instance::init(|_, _, _| {}))
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let cfg = load_config(app.handle());
            let (hotkey, check_hotkey) = (cfg.hotkey.clone(), cfg.check_hotkey.clone());
            let auto = cfg.auto_check;
            app.manage(cfg);
            if auto {
                let handle = app.handle().clone();
                std::thread::spawn(move || auto_loop(handle));
            }

            let check_id = check_hotkey.parse::<Shortcut>().map(|s| s.id()).unwrap_or_default();
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(move |app, shortcut, event| {
                        if event.state == ShortcutState::Pressed {
                            on_hotkey(app, shortcut.id() == check_id);
                        }
                    })
                    .build(),
            )?;
            for key in [&hotkey, &check_hotkey] {
                if let Err(e) = app.global_shortcut().register(key.as_str()) {
                    let error = Some(format!("Горячая клавиша {key} недоступна ({e}). Поменяйте её в config.json и перезапустите."));
                    show_popup(app.handle(), capture::Captured { error, ..Default::default() });
                }
            }

            let settings = MenuItem::with_id(app, "config", "Настройки (config.json)", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip(format!("opengramm: {hotkey} окно, {check_hotkey} проверка"))
                .menu(&Menu::with_items(app, &[&settings, &quit])?)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "config" => open_config(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|win, e| {
            if let WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                let _ = win.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![lt_check, rewrite, source_info, badge_show, badge_hide, open_full])
        .run(tauri::generate_context!())
        .expect("error while running opengramm");
}
