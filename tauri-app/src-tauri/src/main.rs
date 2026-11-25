// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod antigravity;
mod backup;
mod config;
mod failover;
mod quota;
mod usage;

use antigravity::{Account, AntigravityManager};
use backup::{ImportSummary, SnapshotEntry};
use config::{AppPreferences, ConfigManager, Profile, ProfilesConfig, NATIVE_KEY};
use failover::FAILOVER_KEY;
use std::path::PathBuf;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    window::{Effect, EffectState, EffectsBuilder},
    AppHandle, Emitter, Manager, PhysicalPosition, Theme, TitleBarStyle, WebviewUrl,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, FilePath};

const POPOVER_W: f64 = 368.0;
const POPOVER_H: f64 = 528.0;

fn parse_theme(theme: &str) -> Option<Theme> {
    match theme {
        "dark" => Some(Theme::Dark),
        "light" => Some(Theme::Light),
        _ => None,
    }
}

fn sync_window_themes(app: &AppHandle, theme: &str) {
    let parsed = parse_theme(theme);
    for label in ["popover", "settings"] {
        if let Some(win) = app.get_webview_window(label) {
            let _ = win.set_theme(parsed);
        }
    }
}

fn mask_email_str(v: &str, mask: bool) -> String {
    if !mask || v.is_empty() {
        return v.to_string();
    }
    if let Some(i) = v.find('@') {
        let (local, dom) = (&v[..i], &v[i + 1..]);
        let prefix: String = if local.chars().count() <= 2 {
            local.chars().take(1).collect()
        } else {
            local.chars().take(3).collect()
        };
        format!("{prefix}***@{dom}")
    } else if v.chars().count() <= 2 {
        "***".to_string()
    } else {
        let prefix: String = v.chars().take(2).collect();
        format!("{prefix}***")
    }
}

fn notify_changed(app: &AppHandle) {
    let _ = build_tray_menu(app);
    let _ = app.emit("state-changed", ());
}

// ---------- Claude Gateway commands ----------

#[tauri::command]
fn get_profiles() -> Result<ProfilesConfig, String> {
    ConfigManager::load_effective_profiles()
}

#[tauri::command]
fn get_native_info() -> Result<serde_json::Value, String> {
    ConfigManager::native_info()
}

#[tauri::command]
async fn get_usage(tz_offset: Option<i64>) -> Result<serde_json::Value, String> {
    let home = ConfigManager::home()?;
    let app_dir = ConfigManager::get_app_dir();
    let tz = tz_offset.unwrap_or(0);
    tauri::async_runtime::spawn_blocking(move || usage::refresh(&home, &app_dir, tz))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn restore_native(app: AppHandle) -> Result<(), String> {
    ConfigManager::restore_native()?;
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
fn save_profile(
    app: AppHandle,
    old_key: Option<String>,
    key: String,
    name: String,
    env: std::collections::HashMap<String, String>,
    model_prefix: Option<String>,
) -> Result<(), String> {
    let profile = Profile {
        name,
        env,
        model_prefix: model_prefix.unwrap_or_default(),
    };
    ConfigManager::save_profile(old_key, &key, profile)?;
    failover::ensure_running();
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
fn remove_profile(app: AppHandle, key: String) -> Result<(), String> {
    ConfigManager::remove_profile(&key)?;
    failover::ensure_running();
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
fn switch_profile(app: AppHandle, key: String) -> Result<(), String> {
    if key == FAILOVER_KEY {
        failover::ensure_running();
    }
    ConfigManager::apply_profile(&key)?;
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
async fn import_legacy(app: AppHandle) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<FilePath>>();
    app.dialog()
        .file()
        .add_filter("JSON", &["json"])
        .pick_file(move |fp| {
            let _ = tx.send(fp);
        });
    let Some(path) = rx.await.ok().flatten().and_then(|fp| fp.into_path().ok()) else {
        return Err("No file selected".into());
    };
    ConfigManager::import_legacy_profiles(&path)?;
    failover::ensure_running();
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
fn remove_all(app: AppHandle) -> Result<(), String> {
    ConfigManager::remove_all()?;
    failover::ensure_running();
    notify_changed(&app);
    Ok(())
}

// ---------- Claude Failover commands ----------

#[tauri::command]
fn get_failover() -> serde_json::Value {
    failover::status()
}

#[tauri::command]
fn save_failover(
    app: AppHandle,
    chain: Vec<String>,
    port: u16,
    cooldown_secs: u64,
) -> Result<serde_json::Value, String> {
    if port < 1024 {
        return Err("Cổng phải từ 1024 đến 65535".into());
    }
    let mut s = failover::load();
    s.chain = chain;
    s.port = port;
    s.cooldown_secs = cooldown_secs.max(5);
    failover::save(&s)?;
    failover::ensure_running();
    if ConfigManager::load_effective_profiles().map(|c| c.active).unwrap_or_default() == FAILOVER_KEY {
        if s.chain.is_empty() {
            let _ = ConfigManager::restore_native();
        } else {
            let _ = ConfigManager::apply_profile(FAILOVER_KEY);
        }
    }
    notify_changed(&app);
    Ok(failover::status())
}

// ---------- Gemini Antigravity commands ----------

#[tauri::command]
async fn antigravity_status(include_running: Option<bool>) -> Result<serde_json::Value, String> {
    let inc = include_running.unwrap_or(true);
    tauri::async_runtime::spawn_blocking(move || AntigravityManager::status_opts(inc))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn get_popover_state(
    include_usage: Option<bool>,
    tz_offset: Option<i64>,
) -> Result<serde_json::Value, String> {
    let want_usage = include_usage.unwrap_or(false);
    let tz = tz_offset.unwrap_or(0);
    tauri::async_runtime::spawn_blocking(move || {
        let prefs = ConfigManager::load_preferences();
        let profiles = ConfigManager::load_effective_profiles().unwrap_or(ProfilesConfig {
            active: String::new(),
            profiles: std::collections::HashMap::new(),
        });
        let native = ConfigManager::native_info().unwrap_or_default();
        let failover = failover::status();
        let ag = AntigravityManager::status_opts(false).unwrap_or_default();
        let quota = quota::load_quota_cache();
        let usage = if want_usage {
            ConfigManager::home()
                .ok()
                .and_then(|home| usage::refresh(&home, &ConfigManager::get_app_dir(), tz).ok())
        } else {
            None
        };
        Ok(serde_json::json!({
            "prefs": prefs,
            "profiles": profiles,
            "native": native,
            "failover": failover,
            "ag": ag,
            "quota": quota,
            "usage": usage,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn antigravity_capture(app: AppHandle, key: Option<String>) -> Result<Account, String> {
    let acc = tauri::async_runtime::spawn_blocking(move || AntigravityManager::capture_current(key))
        .await
        .map_err(|e| e.to_string())??;
    notify_changed(&app);
    Ok(acc)
}

#[tauri::command]
fn antigravity_save(
    app: AppHandle,
    old_key: Option<String>,
    key: String,
    label: String,
    email: String,
    notes: String,
) -> Result<(), String> {
    AntigravityManager::save_account(old_key, &key, &label, &email, &notes)?;
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
fn antigravity_remove(app: AppHandle, key: String) -> Result<(), String> {
    AntigravityManager::remove_account(&key)?;
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
async fn antigravity_switch(app: AppHandle, key: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || AntigravityManager::switch(&key))
        .await
        .map_err(|e| e.to_string())??;
    notify_changed(&app);
    let app_q = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = quota::refresh_antigravity_quota().await;
        let _ = app_q.emit("state-changed", ());
    });
    Ok(())
}

#[tauri::command]
async fn antigravity_switch_next(app: AppHandle) -> Result<Account, String> {
    let acc = tauri::async_runtime::spawn_blocking(AntigravityManager::switch_next)
        .await
        .map_err(|e| e.to_string())??;
    notify_changed(&app);
    let app_q = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = quota::refresh_antigravity_quota().await;
        let _ = app_q.emit("state-changed", ());
    });
    Ok(acc)
}

#[tauri::command]
async fn antigravity_add_another(app: AppHandle) -> Result<Option<Account>, String> {
    let saved = tauri::async_runtime::spawn_blocking(AntigravityManager::add_another)
        .await
        .map_err(|e| e.to_string())??;
    notify_changed(&app);
    Ok(saved)
}

#[tauri::command]
async fn antigravity_clear(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(AntigravityManager::clear_local)
        .await
        .map_err(|e| e.to_string())??;
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
async fn antigravity_restore(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(AntigravityManager::restore_original)
        .await
        .map_err(|e| e.to_string())??;
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
fn antigravity_open(app_name: Option<String>) -> Result<(), String> {
    AntigravityManager::open_app(app_name.as_deref().unwrap_or("Antigravity"))
}

// ---------- Quota telemetry commands ----------

#[tauri::command]
fn get_quota_snapshot() -> quota::QuotaCacheStore {
    quota::load_quota_cache()
}

#[tauri::command]
async fn refresh_all_quotas(
    app: AppHandle,
    allow_keychain: Option<bool>,
) -> Result<quota::QuotaCacheStore, String> {
    let allow_kc = allow_keychain.unwrap_or(false);
    let _ = quota::refresh_antigravity_quota().await;
    let _ = quota::refresh_claude_quota(allow_kc).await;
    let store = quota::load_quota_cache();
    let _ = app.emit("state-changed", ());
    Ok(store)
}

// ---------- Backup / Export & Import commands ----------

#[tauri::command]
async fn export_backup(
    app: AppHandle,
    include_secrets: bool,
    path: Option<String>,
) -> Result<serde_json::Value, String> {
    let target = match path.filter(|p| !p.trim().is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            let (tx, rx) = tokio::sync::oneshot::channel::<Option<FilePath>>();
            let ts = antigravity::now_unix_secs();
            let suggested = format!("claude-antigravity-backup-{ts}.json");
            app.dialog()
                .file()
                .set_file_name(&suggested)
                .add_filter("JSON Backup", &["json"])
                .save_file(move |fp| {
                    let _ = tx.send(fp);
                });
            match rx.await.ok().flatten().and_then(|fp| fp.into_path().ok()) {
                Some(p) => p,
                None => return Ok(serde_json::json!({ "cancelled": true })),
            }
        }
    };
    backup::export_to_file(&target, include_secrets)
}

#[tauri::command]
async fn import_backup(
    app: AppHandle,
    mode: Option<String>,
    path: Option<String>,
) -> Result<serde_json::Value, String> {
    let src = match path.filter(|p| !p.trim().is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            let (tx, rx) = tokio::sync::oneshot::channel::<Option<FilePath>>();
            app.dialog()
                .file()
                .add_filter("JSON Backup / Config", &["json"])
                .pick_file(move |fp| {
                    let _ = tx.send(fp);
                });
            match rx.await.ok().flatten().and_then(|fp| fp.into_path().ok()) {
                Some(p) => p,
                None => return Ok(serde_json::json!({ "cancelled": true })),
            }
        }
    };
    let summary: ImportSummary = backup::import_from_file(&src, mode.as_deref().unwrap_or("merge"))?;
    failover::ensure_running();
    notify_changed(&app);
    Ok(serde_json::to_value(summary).unwrap_or_default())
}

#[tauri::command]
fn create_snapshot(include_secrets: Option<bool>) -> Result<SnapshotEntry, String> {
    backup::create_local_snapshot("manual", include_secrets.unwrap_or(true))
}

#[tauri::command]
fn list_snapshots() -> Vec<SnapshotEntry> {
    backup::list_local_snapshots()
}

#[tauri::command]
fn restore_snapshot(app: AppHandle, path: String, mode: Option<String>) -> Result<ImportSummary, String> {
    let summary = backup::import_from_file(&PathBuf::from(path), mode.as_deref().unwrap_or("replace"))?;
    failover::ensure_running();
    notify_changed(&app);
    Ok(summary)
}

#[tauri::command]
fn delete_snapshot(filename: String) -> Result<(), String> {
    backup::delete_local_snapshot(&filename)
}

// ---------- Preferences & Windows commands ----------

#[tauri::command]
fn get_preferences() -> AppPreferences {
    ConfigManager::load_preferences()
}

#[tauri::command]
fn save_preferences(app: AppHandle, prefs: AppPreferences) -> Result<(), String> {
    ConfigManager::save_preferences(&prefs)?;
    sync_window_themes(&app, &prefs.theme);
    notify_changed(&app);
    Ok(())
}

#[tauri::command]
async fn pick_sync_file(app: AppHandle, default_name: String) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<FilePath>>();
    app.dialog()
        .file()
        .set_file_name(&default_name)
        .add_filter("JSON", &["json"])
        .pick_file(move |fp| {
            let _ = tx.send(fp);
        });
    Ok(rx
        .await
        .ok()
        .flatten()
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
fn open_dashboard(app: AppHandle, section: Option<String>) {
    hide_popover_window(&app);
    open_settings_window(&app, section.as_deref());
}

#[tauri::command]
fn hide_popover(app: AppHandle) {
    hide_popover_window(&app);
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

// ---------- Tray Menu & Window Management ----------

fn build_tray_menu(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let menu = Menu::new(app)?;
    let prefs = ConfigManager::load_preferences();
    let is_vi = prefs.language == "vi";

    // Section 1: Claude Code Gateways
    let claude_hdr = MenuItem::with_id(app, "hdr_claude", "CLAUDE CODE", false, None::<&str>)?;
    menu.append(&claude_hdr)?;

    let cfg = ConfigManager::load_effective_profiles().unwrap_or(ProfilesConfig {
        active: String::new(),
        profiles: std::collections::HashMap::new(),
    });
    let native_active = cfg.active.is_empty();
    let native_item = CheckMenuItem::with_id(
        app,
        format!("profile_{NATIVE_KEY}"),
        if is_vi { "Tài khoản mặc định (Native)" } else { "Default Account (Native)" },
        true,
        native_active,
        None::<&str>,
    )?;
    menu.append(&native_item)?;

    let fo = failover::load();
    if !fo.chain.is_empty() {
        let chain_lbl = fo.chain.join(" → ");
        let fo_lbl = if is_vi {
            format!("Failover tự động ({chain_lbl})")
        } else {
            format!("Auto Failover ({chain_lbl})")
        };
        let fo_item = CheckMenuItem::with_id(
            app,
            format!("profile_{FAILOVER_KEY}"),
            fo_lbl,
            true,
            cfg.active == FAILOVER_KEY,
            None::<&str>,
        )?;
        menu.append(&fo_item)?;
    }

    let mut keys: Vec<_> = cfg.profiles.keys().cloned().collect();
    keys.sort();
    for key in keys {
        if let Some(p) = cfg.profiles.get(&key) {
            let item = CheckMenuItem::with_id(
                app,
                format!("profile_{key}"),
                &p.name,
                true,
                cfg.active == key,
                None::<&str>,
            )?;
            menu.append(&item)?;
        }
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;

    // Section 2: Gemini Antigravity
    let ag_hdr = MenuItem::with_id(app, "hdr_ag", "GEMINI ANTIGRAVITY", false, None::<&str>)?;
    menu.append(&ag_hdr)?;

    let ag_status = AntigravityManager::status_opts(false).ok();
    if let Some(ref st) = ag_status {
        let active_ag = st["active"].as_str().unwrap_or("");
        if let Some(accounts) = st["accounts"].as_array() {
            let mut saved_count = 0;
            for acc in accounts {
                let key = acc["key"].as_str().unwrap_or("");
                let raw_label = acc["label"].as_str().unwrap_or(key);
                let email = acc["email"].as_str().unwrap_or("");
                let label = if prefs.privacy_mode && !email.is_empty() && raw_label.eq_ignore_ascii_case(email) {
                    mask_email_str(raw_label, true)
                } else {
                    raw_label.to_string()
                };
                let shown_email = mask_email_str(email, prefs.privacy_mode);
                let has_login = acc["hasLogin"].as_bool().unwrap_or(false);
                if has_login {
                    saved_count += 1;
                }
                let text = if has_login {
                    if !email.is_empty() && !raw_label.eq_ignore_ascii_case(email) {
                        format!("{label} ({shown_email})")
                    } else {
                        label
                    }
                } else if is_vi {
                    format!("{label} (chưa lưu phiên)")
                } else {
                    format!("{label} (not captured)")
                };
                let item = CheckMenuItem::with_id(
                    app,
                    format!("ag_acc_{key}"),
                    text,
                    has_login,
                    active_ag == key && !key.is_empty(),
                    None::<&str>,
                )?;
                menu.append(&item)?;
            }
            if saved_count >= 2 {
                let next_item = MenuItem::with_id(
                    app,
                    "ag_next",
                    if is_vi { "Chuyển nhanh Antigravity kế tiếp" } else { "Switch to Next Antigravity" },
                    true,
                    None::<&str>,
                )?;
                menu.append(&next_item)?;
            }
        }
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "settings",
        if is_vi { "Mở Dashboard & Cài đặt…" } else { "Open Dashboard & Settings…" },
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "backup",
        if is_vi { "Backup & Import dữ liệu…" } else { "Backup & Import Data…" },
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        if is_vi { "Thoát" } else { "Quit" },
        true,
        None::<&str>,
    )?)?;

    if let Some(tray) = app.tray_by_id("main_tray") {
        tray.set_menu(Some(menu))?;
        let active_cl = if cfg.active.is_empty() {
            "Native".to_string()
        } else if cfg.active == FAILOVER_KEY {
            "Failover".to_string()
        } else {
            cfg.profiles.get(&cfg.active).map(|p| p.name.clone()).unwrap_or(cfg.active)
        };
        let active_ag = ag_status
            .as_ref()
            .and_then(|s| s["active"].as_str().filter(|v| !v.is_empty()).map(str::to_string))
            .unwrap_or_else(|| if is_vi { "Chưa chọn".to_string() } else { "None".to_string() });
        let _ = tray.set_tooltip(Some(format!(
            "Claude: {active_cl} • Antigravity: {active_ag}"
        )));
    }
    Ok(())
}

fn hide_popover_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("popover") {
        let _ = w.hide();
    }
}

fn toggle_popover_window(app: &AppHandle, tray_rect: Option<tauri::Rect>) {
    if let Some(win) = app.get_webview_window("popover") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
            return;
        }
        position_popover(&win, tray_rect);
        let _ = win.show();
        let _ = win.set_focus();
        let _ = win.emit("popover-opened", ());
        return;
    }

    let prefs = ConfigManager::load_preferences();
    let mut builder = WebviewWindowBuilder::new(app, "popover", WebviewUrl::App("popover.html".into()))
        .title("AgentBar")
        .inner_size(POPOVER_W, POPOVER_H)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(true)
        .effects(
            EffectsBuilder::new()
                .effects([Effect::Popover])
                .state(EffectState::Active)
                .radius(13.0)
                .build(),
        )
        .visible(false);
    if let Some(t) = parse_theme(&prefs.theme) {
        builder = builder.theme(Some(t));
    }

    if let Ok(win) = builder.build() {
        let win_clone = win.clone();
        win.on_window_event(move |event| {
            if let WindowEvent::Focused(false) = event {
                let _ = win_clone.hide();
            }
        });
        position_popover(&win, tray_rect);
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn position_popover(win: &tauri::WebviewWindow, tray_rect: Option<tauri::Rect>) {
    let scale = win
        .current_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .unwrap_or(2.0);
    let win_w_phys = (POPOVER_W * scale) as i32;

    if let Some(rect) = tray_rect {
        let (rx, ry, rw, rh) = match (rect.position, rect.size) {
            (tauri::Position::Physical(p), tauri::Size::Physical(s)) => {
                (p.x, p.y, s.width as i32, s.height as i32)
            }
            (tauri::Position::Logical(p), tauri::Size::Logical(s)) => (
                (p.x * scale) as i32,
                (p.y * scale) as i32,
                (s.width * scale) as i32,
                (s.height * scale) as i32,
            ),
            (tauri::Position::Physical(p), tauri::Size::Logical(s)) => {
                (p.x, p.y, (s.width * scale) as i32, (s.height * scale) as i32)
            }
            (tauri::Position::Logical(p), tauri::Size::Physical(s)) => {
                ((p.x * scale) as i32, (p.y * scale) as i32, s.width as i32, s.height as i32)
            }
        };

        let mut x = rx + (rw / 2) - (win_w_phys / 2);
        let y = ry + rh + (6.0 * scale) as i32;

        if let Ok(Some(mon)) = win.current_monitor() {
            let mon_pos = mon.position();
            let mon_size = mon.size();
            let margin = (10.0 * scale) as i32;
            let min_x = mon_pos.x + margin;
            let max_x = mon_pos.x + mon_size.width as i32 - win_w_phys - margin;
            if x < min_x {
                x = min_x;
            } else if x > max_x {
                x = max_x;
            }
        }
        let _ = win.set_position(PhysicalPosition::new(x, y));
    }
}

fn open_settings_window(app: &AppHandle, section: Option<&str>) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        if let Some(sec) = section {
            let _ = win.emit("navigate-section", sec);
        }
        return;
    }

    let url = match section {
        Some(sec) => format!("index.html#{sec}"),
        None => "index.html".to_string(),
    };
    let prefs = ConfigManager::load_preferences();
    let theme_opt = parse_theme(&prefs.theme);
    let make_builder = || {
        let b = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App(url.clone().into()))
            .title("AgentBar")
            .inner_size(940.0, 690.0)
            .min_inner_size(740.0, 520.0)
            .title_bar_style(TitleBarStyle::Overlay)
            .hidden_title(true)
            .transparent(true)
            .effects(
                EffectsBuilder::new()
                    .effects([Effect::Sidebar])
                    .state(EffectState::Active)
                    .build(),
            )
            .center();
        match theme_opt {
            Some(t) => b.theme(Some(t)),
            None => b,
        }
    };
    let builder = match app.default_window_icon().cloned() {
        Some(icon) => make_builder().icon(icon).unwrap_or_else(|_| make_builder()),
        None => make_builder(),
    };
    if let Ok(win) = builder.build() {
        let _ = win.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_profiles,
            get_native_info,
            get_usage,
            restore_native,
            save_profile,
            remove_profile,
            switch_profile,
            import_legacy,
            remove_all,
            get_failover,
            save_failover,
            antigravity_status,
            get_popover_state,
            antigravity_capture,
            antigravity_save,
            antigravity_remove,
            antigravity_switch,
            antigravity_switch_next,
            antigravity_add_another,
            antigravity_clear,
            antigravity_restore,
            antigravity_open,
            get_quota_snapshot,
            refresh_all_quotas,
            export_backup,
            import_backup,
            create_snapshot,
            list_snapshots,
            restore_snapshot,
            delete_snapshot,
            get_preferences,
            save_preferences,
            pick_sync_file,
            open_dashboard,
            hide_popover,
            quit_app,
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            failover::ensure_running();

            let tray_icon = tauri::image::Image::new(include_bytes!("../icons/tray.rgba"), 64, 64);
            TrayIconBuilder::with_id("main_tray")
                .icon(tray_icon)
                .icon_as_template(true)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        toggle_popover_window(tray.app_handle(), Some(rect));
                    }
                })
                .on_menu_event(|app, event| {
                    let id = event.id().as_ref();
                    if id == "quit" {
                        app.exit(0);
                    } else if id == "settings" {
                        open_settings_window(app, None);
                    } else if id == "backup" {
                        open_settings_window(app, Some("backup"));
                    } else if id == "ag_next" {
                        let app_h = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = tauri::async_runtime::spawn_blocking(
                                AntigravityManager::switch_next,
                            )
                            .await;
                            notify_changed(&app_h);
                        });
                    } else if let Some(key) = id.strip_prefix("ag_acc_") {
                        let k = key.to_string();
                        let app_h = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = tauri::async_runtime::spawn_blocking(move || {
                                AntigravityManager::switch(&k)
                            })
                            .await;
                            notify_changed(&app_h);
                        });
                    } else if let Some(key) = id.strip_prefix("profile_") {
                        if key == FAILOVER_KEY {
                            failover::ensure_running();
                        }
                        if ConfigManager::apply_profile(key).is_ok() {
                            notify_changed(app);
                        }
                    }
                })
                .build(app)?;

            build_tray_menu(app.handle())?;

            // Background initial quota warm-up (non-blocking, no Keychain prompt)
            let app_bg = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let _ = quota::refresh_antigravity_quota().await;
                let _ = quota::refresh_claude_quota(false).await;
                let _ = app_bg.emit("state-changed", ());
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|_app_handle, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
