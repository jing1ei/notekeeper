use crate::config::atomic_write;
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{watch, Mutex};

/// Live status for one bot, surfaced to the UI and the tray. The message
/// counters and the long-poll `offset` are persisted across restarts; the
/// transient fields (`running`, `username`, `last_error`) are not.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct BotStatus {
    #[serde(default, skip_deserializing)]
    pub running: bool,
    #[serde(default, skip_deserializing)]
    pub username: Option<String>,
    #[serde(default, skip_deserializing)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_message_at: Option<String>,
    #[serde(default)]
    pub message_count: u64,
    /// Next Telegram getUpdates offset. Persisted so a restart doesn't
    /// re-deliver (and re-append) already-saved messages.
    #[serde(default)]
    pub offset: i64,
}

pub type StatusMap = Arc<Mutex<HashMap<String, BotStatus>>>;

/// Remove the bot token from a message before it is shown or stored.
///
/// `reqwest` includes the full request URL in its error `Display`, and our URLs
/// embed the token (`/bot<TOKEN>/getUpdates`). Without this, a network error
/// would surface the token in the UI, the tray tooltip, logs, and screenshots.
fn scrub(msg: String, token: &str) -> String {
    if token.is_empty() {
        msg
    } else {
        msg.replace(token, "<redacted>")
    }
}

/// Handle used to stop a running bot task.
pub struct BotHandle {
    pub stop: watch::Sender<bool>,
}

/// Load persisted per-bot status (counters + offset) from disk.
pub fn load_status(path: &Path) -> HashMap<String, BotStatus> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// Persist the current status map atomically.
pub async fn persist_status(path: &Path, status: &StatusMap) {
    let snapshot = { status.lock().await.clone() };
    if let Ok(text) = serde_json::to_string_pretty(&snapshot) {
        let _ = atomic_write(path, text.as_bytes());
    }
}

async fn set_status<F: FnOnce(&mut BotStatus)>(status: &StatusMap, id: &str, f: F) {
    let mut map = status.lock().await;
    let entry = map.entry(id.to_string()).or_default();
    f(entry);
}

/// Call getMe to validate a token and return the bot's @username.
pub async fn get_me(client: &reqwest::Client, token: &str) -> Result<String, String> {
    let url = format!("https://api.telegram.org/bot{}/getMe", token);
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| scrub(e.to_string(), token))?;
    let json: Value = resp.json().await.map_err(|e| scrub(e.to_string(), token))?;
    if json["ok"].as_bool() != Some(true) {
        return Err(json["description"]
            .as_str()
            .unwrap_or("invalid token")
            .to_string());
    }
    Ok(json["result"]["username"]
        .as_str()
        .unwrap_or("unknown")
        .to_string())
}

pub async fn append_timestamped(path: &str, text: &str) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let ts = Local::now().format("%Y-%m-%d %H:%M").to_string();
    let line = format!("- [{}] {}\n", ts, text);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    file.flush().await
}

async fn react(client: &reqwest::Client, token: &str, chat_id: i64, message_id: i64) {
    let url = format!("https://api.telegram.org/bot{}/setMessageReaction", token);
    let body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reaction": [{ "type": "emoji", "emoji": "👍" }]
    });
    let _ = client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await;
}

/// Long-poll loop for a single bot. Runs until `stop_rx` flips to true.
#[allow(clippy::too_many_arguments)]
pub async fn run_bot(
    id: String,
    token: String,
    file: String,
    allowed_user_id: i64,
    status: StatusMap,
    status_path: PathBuf,
    client: reqwest::Client,
    mut stop_rx: watch::Receiver<bool>,
) {
    // Validate token / fetch username up front.
    match get_me(&client, &token).await {
        Ok(name) => {
            set_status(&status, &id, |s| {
                s.username = Some(name);
                s.last_error = None;
                s.running = true;
            })
            .await;
        }
        Err(e) => {
            set_status(&status, &id, |s| {
                s.running = true;
                s.last_error = Some(format!("getMe failed: {}", e));
            })
            .await;
        }
    }

    // Resume from the persisted offset so a restart doesn't replay old messages.
    let mut offset: i64 = { status.lock().await.get(&id).map(|s| s.offset).unwrap_or(0) };
    let updates_url = format!("https://api.telegram.org/bot{}/getUpdates", token);

    loop {
        if *stop_rx.borrow() {
            break;
        }

        let params: Vec<(&str, String)> = vec![
            ("offset", offset.to_string()),
            ("timeout", "30".to_string()),
            ("allowed_updates", "[\"message\"]".to_string()),
        ];

        let request = client
            .get(&updates_url)
            .query(&params)
            .timeout(Duration::from_secs(45))
            .send();

        let result = tokio::select! {
            _ = stop_rx.changed() => { break; }
            r = request => r,
        };

        match result {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(json) => {
                    if json["ok"].as_bool() != Some(true) {
                        let code = json["error_code"].as_i64().unwrap_or(0);
                        let desc = json["description"]
                            .as_str()
                            .unwrap_or("getUpdates error")
                            .to_string();
                        let msg = if code == 409 {
                            format!("conflict: this token is already being polled elsewhere ({desc})")
                        } else {
                            desc
                        };
                        set_status(&status, &id, |s| s.last_error = Some(msg)).await;
                        // 409s won't clear on their own; back off a little longer.
                        let wait = if code == 409 { 15 } else { 5 };
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    }
                    set_status(&status, &id, |s| {
                        s.last_error = None;
                        s.running = true;
                    })
                    .await;

                    if let Some(updates) = json["result"].as_array() {
                        let start_offset = offset;
                        let mut write_failed = false;
                        for update in updates {
                            let update_id = update["update_id"].as_i64();
                            let msg = &update["message"];
                            let text = match msg["text"].as_str() {
                                Some(t) => t.to_string(),
                                None => {
                                    // Non-text update: nothing to save, but confirm it
                                    // so Telegram doesn't keep re-delivering it.
                                    if let Some(uid) = update_id {
                                        offset = offset.max(uid + 1);
                                    }
                                    continue;
                                }
                            };
                            let from_id = msg["from"]["id"].as_i64().unwrap_or(0);
                            let chat_id = msg["chat"]["id"].as_i64().unwrap_or(from_id);
                            let message_id = msg["message_id"].as_i64().unwrap_or(0);

                            if allowed_user_id != 0 && from_id != allowed_user_id {
                                // Not the owner — ignore, but confirm so it isn't replayed.
                                if let Some(uid) = update_id {
                                    offset = offset.max(uid + 1);
                                }
                                continue;
                            }

                            match append_timestamped(&file, &text).await {
                                Ok(()) => {
                                    // Only advance past a message once it's safely written.
                                    if let Some(uid) = update_id {
                                        offset = offset.max(uid + 1);
                                    }
                                    set_status(&status, &id, |s| {
                                        s.message_count += 1;
                                        s.last_message_at =
                                            Some(Local::now().format("%H:%M").to_string());
                                        s.last_error = None;
                                    })
                                    .await;
                                    react(&client, &token, chat_id, message_id).await;
                                }
                                Err(e) => {
                                    // Do NOT advance the offset: leaving this update
                                    // unconfirmed makes Telegram re-deliver it next poll
                                    // so a transient write failure can't silently drop a note.
                                    set_status(&status, &id, |s| {
                                        s.last_error = Some(format!("write failed: {}", e));
                                    })
                                    .await;
                                    write_failed = true;
                                    break;
                                }
                            }
                        }
                        // Record the advanced offset + counters so a restart resumes
                        // cleanly without re-delivering already-saved messages.
                        if offset != start_offset {
                            set_status(&status, &id, |s| s.offset = offset).await;
                            persist_status(&status_path, &status).await;
                        }
                        // Back off briefly on a write failure so we don't hammer Telegram
                        // re-fetching the same un-writable message in a tight loop.
                        if write_failed {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
                Err(e) => {
                    let msg = scrub(e.to_string(), &token);
                    set_status(&status, &id, |s| s.last_error = Some(msg)).await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            },
            Err(e) => {
                // Network/timeout. Long-poll timeouts are normal; only record real errors.
                if !e.is_timeout() {
                    let msg = scrub(e.to_string(), &token);
                    set_status(&status, &id, |s| s.last_error = Some(msg)).await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    // Note: we intentionally do NOT clear `running` here. This loop only exits
    // after a stop signal, and `stop_bot` already sets `running = false`
    // synchronously. Clearing it again here would race with a freshly started
    // task during a restart (edit/toggle), clobbering the new task's
    // `running = true` back to `false`.
}
