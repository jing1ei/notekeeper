#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bots;
mod config;
mod secrets;

use bots::{run_bot, BotHandle, BotStatus, StatusMap};
use chrono::Local;
use config::{BotConfig, Config};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tokio::sync::{watch, Mutex};
use uuid::Uuid;

pub struct AppState {
    pub config_path: PathBuf,
    /// Where persisted per-bot status (counters + long-poll offset) lives.
    pub status_path: PathBuf,
    pub config: Mutex<Config>,
    pub handles: Mutex<HashMap<String, BotHandle>>,
    pub status: StatusMap,
    /// Shared HTTP client, reused across all bots and token validation.
    pub http: reqwest::Client,
    /// Registered hotkey -> bot id, for local quick-capture.
    pub shortcuts: Mutex<HashMap<Shortcut, String>>,
    /// Which bot the quick-capture window should write to right now.
    pub quick_target: Mutex<Option<(String, String)>>,
}

#[derive(Serialize)]
struct BotView {
    id: String,
    name: String,
    token: String,
    file: String,
    allowed_user_id: i64,
    enabled: bool,
    shortcut: Option<String>,
    status: BotStatus,
}

#[derive(Serialize, Clone)]
struct QuickTarget {
    id: String,
    name: String,
}

// ---- bot lifecycle helpers ----

async fn start_bot(bot: &BotConfig, state: &AppState) {
    stop_bot(&bot.id, state).await;

    let (tx, rx) = watch::channel(false);
    state
        .handles
        .lock()
        .await
        .insert(bot.id.clone(), BotHandle { stop: tx });
    {
        let mut s = state.status.lock().await;
        s.entry(bot.id.clone()).or_default().running = true;
    }

    let id = bot.id.clone();
    let token = bot.token.clone();
    let file = bot.file.clone();
    let allowed = bot.allowed_user_id;
    let status = state.status.clone();
    let status_path = state.status_path.clone();
    let client = state.http.clone();
    tauri::async_runtime::spawn(async move {
        run_bot(id, token, file, allowed, status, status_path, client, rx).await;
    });
}

async fn stop_bot(id: &str, state: &AppState) {
    if let Some(h) = state.handles.lock().await.remove(id) {
        let _ = h.stop.send(true);
    }
    if let Some(s) = state.status.lock().await.get_mut(id) {
        s.running = false;
    }
}

// ---- global shortcuts ----

/// Re-register all configured hotkeys from the current config.
async fn sync_shortcuts(app: &tauri::AppHandle) {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();

    let bots = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().await;
        cfg.bots.clone()
    };

    let mut map: HashMap<Shortcut, String> = HashMap::new();
    for b in bots {
        let Some(sc) = b.shortcut.as_ref() else { continue };
        if sc.trim().is_empty() {
            continue;
        }
        if let Ok(parsed) = sc.parse::<Shortcut>() {
            if gs.register(parsed.clone()).is_ok() {
                map.insert(parsed, b.id.clone());
            }
        }
    }

    let state = app.state::<AppState>();
    *state.shortcuts.lock().await = map;
}

/// When a hotkey fires, show the quick-capture window targeting its bot.
async fn open_quick_for_shortcut(app: &tauri::AppHandle, sc: &Shortcut) {
    let state = app.state::<AppState>();
    let id = { state.shortcuts.lock().await.get(sc).cloned() };
    let Some(id) = id else { return };
    let name = {
        let cfg = state.config.lock().await;
        cfg.bots
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.name.clone())
            .unwrap_or_default()
    };
    *state.quick_target.lock().await = Some((id.clone(), name.clone()));

    if let Some(w) = app.get_webview_window("quick") {
        let _ = w.show();
        let _ = w.set_focus();
        let _ = w.emit("quick-open", QuickTarget { id, name });
    }
}

// ---- commands ----

#[tauri::command]
async fn get_bots(state: State<'_, AppState>) -> Result<Vec<BotView>, String> {
    let config = state.config.lock().await;
    let status = state.status.lock().await;
    let out = config
        .bots
        .iter()
        .map(|b| BotView {
            id: b.id.clone(),
            name: b.name.clone(),
            token: b.token.clone(),
            file: b.file.clone(),
            allowed_user_id: b.allowed_user_id,
            enabled: b.enabled,
            shortcut: b.shortcut.clone(),
            status: status.get(&b.id).cloned().unwrap_or_default(),
        })
        .collect();
    Ok(out)
}

#[tauri::command]
async fn add_bot(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    name: String,
    token: String,
    file: String,
    allowed_user_id: i64,
    enabled: bool,
    shortcut: Option<String>,
) -> Result<(), String> {
    let bot = BotConfig {
        id: Uuid::new_v4().to_string(),
        name,
        token,
        file,
        allowed_user_id,
        enabled,
        shortcut,
    };
    // Store the token in the Keychain before persisting the (token-less) config.
    secrets::set_token(&bot.id, &bot.token)?;
    {
        let mut config = state.config.lock().await;
        config.bots.push(bot.clone());
        config.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    if bot.enabled {
        start_bot(&bot, state.inner()).await;
    }
    sync_shortcuts(&app).await;
    Ok(())
}

#[tauri::command]
async fn update_bot(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
    name: String,
    token: String,
    file: String,
    allowed_user_id: i64,
    enabled: bool,
    shortcut: Option<String>,
) -> Result<(), String> {
    let updated;
    {
        let mut config = state.config.lock().await;
        let b = config
            .bots
            .iter_mut()
            .find(|b| b.id == id)
            .ok_or_else(|| "bot not found".to_string())?;
        b.name = name;
        b.token = token;
        b.file = file;
        b.allowed_user_id = allowed_user_id;
        b.enabled = enabled;
        b.shortcut = shortcut;
        updated = b.clone();
        secrets::set_token(&updated.id, &updated.token)?;
        config.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    stop_bot(&id, state.inner()).await;
    if updated.enabled {
        start_bot(&updated, state.inner()).await;
    }
    sync_shortcuts(&app).await;
    Ok(())
}

#[tauri::command]
async fn remove_bot(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    stop_bot(&id, state.inner()).await;
    {
        let mut config = state.config.lock().await;
        config.bots.retain(|b| b.id != id);
        config.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    // Remove the bot's token from the Keychain so it doesn't linger.
    secrets::delete_token(&id);
    state.status.lock().await.remove(&id);
    // Persist so the removed bot's counters/offset don't linger in status.json
    // and get reloaded as an orphan entry on the next launch.
    bots::persist_status(&state.status_path, &state.status).await;
    sync_shortcuts(&app).await;
    Ok(())
}

#[tauri::command]
async fn set_enabled(state: State<'_, AppState>, id: String, enabled: bool) -> Result<(), String> {
    let bot;
    {
        let mut config = state.config.lock().await;
        let b = config
            .bots
            .iter_mut()
            .find(|b| b.id == id)
            .ok_or_else(|| "bot not found".to_string())?;
        b.enabled = enabled;
        bot = b.clone();
        config.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    if enabled {
        start_bot(&bot, state.inner()).await;
    } else {
        stop_bot(&id, state.inner()).await;
    }
    Ok(())
}

#[tauri::command]
async fn validate_token(state: State<'_, AppState>, token: String) -> Result<String, String> {
    bots::get_me(&state.http, &token).await
}

/// Async so the blocking native dialog runs off the main thread (calling the
/// blocking picker on the main thread can freeze the UI).
#[tauri::command]
async fn pick_markdown_file(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Markdown / text", &["md", "markdown", "txt"])
        .pick_file(move |picked| {
            let _ = tx.send(picked);
        });
    rx.await
        .ok()
        .flatten()
        .and_then(|p| p.into_path().ok())
        .map(|pb| pb.to_string_lossy().to_string())
}

/// Open a bot's markdown file in the system's default text editor.
/// The file is created (empty) if it doesn't exist yet, so the dashboard link
/// always works even before the first message has been received.
#[tauri::command]
async fn open_note_file(file: String) -> Result<(), String> {
    if file.trim().is_empty() {
        return Err("no file path set for this bot".into());
    }
    let path = PathBuf::from(&file);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| e.to_string())?;
    }
    // `open -t` opens with the default *text* editor (e.g. TextEdit) rather than
    // a Markdown previewer, so the file is immediately editable.
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-t")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        return Err("opening files is only supported on macOS".into());
    }
    Ok(())
}

/// Returns the bot the quick-capture window should currently write to.
#[tauri::command]
async fn get_quick_target(state: State<'_, AppState>) -> Result<Option<QuickTarget>, String> {
    Ok(state
        .quick_target
        .lock()
        .await
        .clone()
        .map(|(id, name)| QuickTarget { id, name }))
}

/// Append text to a bot's file locally (used by the quick-capture window).
#[tauri::command]
async fn append_note(state: State<'_, AppState>, id: String, text: String) -> Result<(), String> {
    let file = {
        let cfg = state.config.lock().await;
        cfg.bots
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.file.clone())
    };
    let Some(file) = file else {
        return Err("bot not found".into());
    };
    bots::append_timestamped(&file, &text)
        .await
        .map_err(|e| e.to_string())?;
    {
        let mut map = state.status.lock().await;
        let e = map.entry(id).or_default();
        e.message_count += 1;
        e.last_message_at = Some(Local::now().format("%H:%M").to_string());
    }
    bots::persist_status(&state.status_path, &state.status).await;
    Ok(())
}

// ---- tray ----

/// A cheap fingerprint of everything the tray displays, so we only rebuild the
/// menu when something actually changed (rebuilding every tick causes flicker
/// and can dismiss the menu while it's open).
async fn tray_signature(app: &tauri::AppHandle) -> String {
    let state = app.state::<AppState>();
    let config = state.config.lock().await;
    let status = state.status.lock().await;
    let mut sig = String::new();
    for b in &config.bots {
        let st = status.get(&b.id).cloned().unwrap_or_default();
        sig.push_str(&format!(
            "{}|{}|{}|{}|{}|{};",
            b.id,
            b.name,
            b.enabled,
            st.running,
            st.last_error.is_some(),
            st.message_count,
        ));
    }
    sig
}

async fn update_tray(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let (bots, statuses) = {
        let config = state.config.lock().await;
        let status = state.status.lock().await;
        (config.bots.clone(), status.clone())
    };

    let open_i = match MenuItem::with_id(app, "open", "Open Manager", true, None::<&str>) {
        Ok(i) => i,
        Err(_) => return,
    };
    let sep1 = PredefinedMenuItem::separator(app).ok();
    let quit_i = match MenuItem::with_id(app, "quit", "Quit Notekeeper", true, None::<&str>) {
        Ok(i) => i,
        Err(_) => return,
    };

    let mut running = 0usize;
    let mut errors = 0usize;
    let mut status_items: Vec<MenuItem<tauri::Wry>> = Vec::new();
    for b in &bots {
        let st = statuses.get(&b.id).cloned().unwrap_or_default();
        let icon = if !b.enabled {
            "⚪"
        } else if st.last_error.is_some() {
            "🔴"
        } else if st.running {
            "🟢"
        } else {
            "🟡"
        };
        if b.enabled && st.running && st.last_error.is_none() {
            running += 1;
        }
        if b.enabled && st.last_error.is_some() {
            errors += 1;
        }
        let label = format!("{} {}  ·  {} saved", icon, b.name, st.message_count);
        if let Ok(item) = MenuItem::with_id(app, format!("bot_{}", b.id), label, false, None::<&str>)
        {
            status_items.push(item);
        }
    }

    let sep2 = PredefinedMenuItem::separator(app).ok();

    let mut refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&open_i];
    if let Some(s) = sep1.as_ref() {
        refs.push(s);
    }
    for it in &status_items {
        refs.push(it);
    }
    if let Some(s) = sep2.as_ref() {
        refs.push(s);
    }
    refs.push(&quit_i);

    if let Ok(menu) = Menu::with_items(app, refs.as_slice()) {
        if let Some(tray) = app.tray_by_id("tray") {
            let _ = tray.set_menu(Some(menu));
            let tip = format!("Notekeeper — {} running, {} error(s)", running, errors);
            let _ = tray.set_tooltip(Some(tip.as_str()));
        }
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        let app = app.clone();
                        let sc = shortcut.clone();
                        tauri::async_runtime::spawn(async move {
                            open_quick_for_shortcut(&app, &sc).await;
                        });
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_bots,
            add_bot,
            update_bot,
            remove_bot,
            set_enabled,
            validate_token,
            pick_markdown_file,
            open_note_file,
            get_quick_target,
            append_note
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let config_dir = app.path().app_config_dir().expect("no config dir");
            std::fs::create_dir_all(&config_dir).ok();
            let config_path = config_dir.join("bots.json");
            let status_path = config_dir.join("status.json");

            // Restore persisted counters + offsets from the last run.
            let status = bots::load_status(&status_path);

            // Load config, then pull each bot's token from the Keychain. If an
            // older plaintext config still held tokens inline, they're migrated
            // into the Keychain and the config is re-saved without them.
            let mut config = Config::load(&config_path);
            if secrets::hydrate_tokens(&mut config) {
                let _ = config.save(&config_path);
            }

            app.manage(AppState {
                config_path: config_path.clone(),
                status_path,
                config: Mutex::new(config),
                handles: Mutex::new(HashMap::new()),
                status: Arc::new(Mutex::new(status)),
                http: reqwest::Client::new(),
                shortcuts: Mutex::new(HashMap::new()),
                quick_target: Mutex::new(None),
            });

            // Tray icon with an initial menu.
            let open_i = MenuItem::with_id(app, "open", "Open Manager", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Notekeeper", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &quit_i])?;
            let _tray = TrayIconBuilder::with_id("tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Notekeeper")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Closing a window hides it instead of quitting the app.
            for label in ["main", "quick"] {
                if let Some(window) = app.get_webview_window(label) {
                    let w = window.clone();
                    window.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            api.prevent_close();
                            let _ = w.hide();
                        }
                    });
                }
            }

            // Start enabled bots, register hotkeys, then refresh the tray on a timer.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                {
                    let state = handle.state::<AppState>();
                    let bots: Vec<BotConfig> = {
                        let c = state.config.lock().await;
                        c.bots.clone()
                    };
                    for b in bots.iter().filter(|b| b.enabled) {
                        start_bot(b, state.inner()).await;
                    }
                }
                sync_shortcuts(&handle).await;
                let mut last_sig: Option<String> = None;
                loop {
                    let sig = tray_signature(&handle).await;
                    if last_sig.as_ref() != Some(&sig) {
                        update_tray(&handle).await;
                        last_sig = Some(sig);
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running notekeeper");
}
