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
/// `reqwest` includes the token-bearing request URL in its error `Display`, so
/// without this a network error would leak the token into the UI, tray, and logs.
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

/// Strip anything that isn't a safe filename character. Telegram-supplied names
/// can contain path separators or other surprises, so we keep only a basename
/// and a conservative character set.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed
    }
}

/// Pull the first downloadable attachment out of a message, returning its
/// Telegram `file_id` and a suggested filename (when the message provides one).
/// Checks the common attachment kinds in priority order.
fn extract_attachment(msg: &Value) -> Option<(String, Option<String>)> {
    for key in ["document", "video", "audio", "voice", "animation", "video_note"] {
        if let Some(obj) = msg.get(key) {
            if let Some(id) = obj["file_id"].as_str() {
                let name = obj["file_name"].as_str().map(|s| s.to_string());
                return Some((id.to_string(), name));
            }
        }
    }
    // Photos arrive as an array of sizes; the last entry is the largest.
    if let Some(sizes) = msg["photo"].as_array() {
        if let Some(largest) = sizes.last() {
            if let Some(id) = largest["file_id"].as_str() {
                return Some((id.to_string(), None));
            }
        }
    }
    // Stickers are .webp (or .tgs/.webm for animated) with no file_name.
    if let Some(id) = msg["sticker"]["file_id"].as_str() {
        return Some((id.to_string(), None));
    }
    None
}

/// Pick a unique destination path inside `dir`, prefixing the name with a
/// timestamp and appending a counter if a same-named file already exists.
fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let base = format!("{}_{}", stamp, sanitize_filename(name));
    let mut candidate = dir.join(&base);
    let mut n = 1;
    while candidate.exists() {
        // Insert the counter before the extension if there is one.
        let (stem, ext) = match base.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), format!(".{}", e)),
            None => (base.clone(), String::new()),
        };
        candidate = dir.join(format!("{}-{}{}", stem, n, ext));
        n += 1;
    }
    candidate
}

/// A failed download. `permanent` means Telegram itself rejected the file (e.g.
/// it exceeds getFile's ~20 MB download limit), so retrying can't help and the
/// caller should skip past the message instead of blocking on it.
struct DownloadError {
    permanent: bool,
    msg: String,
}

impl DownloadError {
    fn transient(msg: String) -> Self {
        Self { permanent: false, msg }
    }
    fn permanent(msg: String) -> Self {
        Self { permanent: true, msg }
    }
}

/// Download a Telegram file by `file_id` into `dir`, returning the saved path.
async fn download_attachment(
    client: &reqwest::Client,
    token: &str,
    file_id: &str,
    file_name: Option<&str>,
    dir: &Path,
) -> Result<PathBuf, DownloadError> {
    // getFile resolves a file_id to a temporary download path on Telegram's servers.
    let get_url = format!("https://api.telegram.org/bot{}/getFile", token);
    let resp = client
        .get(&get_url)
        .query(&[("file_id", file_id)])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| DownloadError::transient(scrub(e.to_string(), token)))?;
    let json: Value = resp
        .json()
        .await
        .map_err(|e| DownloadError::transient(scrub(e.to_string(), token)))?;
    if json["ok"].as_bool() != Some(true) {
        return Err(DownloadError::permanent(
            json["description"]
                .as_str()
                .unwrap_or("getFile failed")
                .to_string(),
        ));
    }
    let file_path = json["result"]["file_path"]
        .as_str()
        .ok_or_else(|| DownloadError::permanent("getFile returned no file_path".to_string()))?
        .to_string();

    // Fall back to the basename Telegram reports when the message had no file_name.
    let name = file_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| file_path.rsplit('/').next().unwrap_or("file").to_string());

    let dl_url = format!("https://api.telegram.org/file/bot{}/{}", token, file_path);
    let resp = client
        .get(&dl_url)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| DownloadError::transient(scrub(e.to_string(), token)))?;
    let status = resp.status();
    if !status.is_success() {
        // 4xx means Telegram won't serve this path (expired/invalid) — retrying
        // can't help, so treat it as permanent and skip past the message. 5xx and
        // the like are transient and worth retrying.
        let msg = format!("file download returned HTTP {}", status.as_u16());
        return Err(if status.is_client_error() {
            DownloadError::permanent(msg)
        } else {
            DownloadError::transient(msg)
        });
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| DownloadError::transient(scrub(e.to_string(), token)))?;

    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| DownloadError::transient(e.to_string()))?;
    let dest = unique_dest(dir, &name);
    tokio::fs::write(&dest, &bytes)
        .await
        .map_err(|e| DownloadError::transient(e.to_string()))?;
    Ok(dest)
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
    files_dir: Option<String>,
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

    // Resolve where received files are saved: the configured folder, or an
    // `attachments` folder next to the markdown file when unset.
    let save_dir: PathBuf = match files_dir.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => Path::new(&file)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("attachments"))
            .unwrap_or_else(|| PathBuf::from("attachments")),
    };

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

                            // Decide what to write. A plain text message appends its
                            // text; a file/photo/etc is downloaded to `save_dir` and a
                            // note recording the saved path (plus any caption) is
                            // appended. Anything else is acknowledged but not saved.
                            let caption = msg["caption"].as_str().unwrap_or("").trim();
                            let line = if let Some(t) = msg["text"].as_str() {
                                Some(t.to_string())
                            } else if let Some((file_id, file_name)) = extract_attachment(msg) {
                                match download_attachment(
                                    &client,
                                    &token,
                                    &file_id,
                                    file_name.as_deref(),
                                    &save_dir,
                                )
                                .await
                                {
                                    Ok(dest) => {
                                        let name = dest
                                            .file_name()
                                            .map(|n| n.to_string_lossy().to_string())
                                            .unwrap_or_else(|| "file".to_string());
                                        let mut note =
                                            format!("saved file: {} → {}", name, dest.display());
                                        if !caption.is_empty() {
                                            note.push_str(&format!(" — {}", caption));
                                        }
                                        Some(note)
                                    }
                                    Err(e) if e.permanent => {
                                        // Telegram will never serve this file (usually >20 MB).
                                        // Record a note and let the offset advance so it can't
                                        // block every later message forever.
                                        let mut note =
                                            format!("could not save file: {}", e.msg);
                                        if !caption.is_empty() {
                                            note.push_str(&format!(" — {}", caption));
                                        }
                                        Some(note)
                                    }
                                    Err(e) => {
                                        // Transient (network/disk) failure: record the error
                                        // and don't advance the offset, so Telegram re-delivers
                                        // and we retry.
                                        set_status(&status, &id, |s| {
                                            s.last_error =
                                                Some(format!("file save failed: {}", e.msg));
                                        })
                                        .await;
                                        write_failed = true;
                                        break;
                                    }
                                }
                            } else {
                                // Non-text, non-file update: nothing to save, but confirm
                                // it so Telegram doesn't keep re-delivering it.
                                if let Some(uid) = update_id {
                                    offset = offset.max(uid + 1);
                                }
                                continue;
                            };
                            let Some(text) = line else { continue };

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

    // Deliberately don't clear `running` here: `stop_bot` already did so before
    // signalling stop, and clearing it again would race a freshly started task
    // during a restart, flipping its `running = true` back to false.
}
