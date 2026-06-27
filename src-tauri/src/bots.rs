use crate::config::atomic_write;
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch, Mutex};

/// getFile over the public Bot API only serves files up to 20 MB. A self-hosted
/// local server lifts this, so we only pre-empt the limit when talking to the
/// public API.
///
/// Telegram documents this as 20 MB in *decimal* (20,000,000 bytes), not 20 MiB.
/// Using the decimal value keeps our pre-emptive "too big" message in step with
/// the server's real cutoff — a file in the 20,000,000..20,971,520 window would
/// otherwise slip past this check and come back as a generic getFile failure
/// instead of the friendly "over the 20 MB limit" note.
const PUBLIC_DOWNLOAD_LIMIT: i64 = 20_000_000;

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
    /// Error from the daily send-back, kept separate from `last_error` so the
    /// poll loop (which clears `last_error` on every successful getUpdates) can't
    /// wipe it within seconds and hide a failed daily send. Transient.
    #[serde(default, skip_deserializing)]
    pub last_daily_error: Option<String>,
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

/// A file download that's been acknowledged to Telegram but not yet saved.
///
/// Downloads run on a background worker so a large (slow) file can't block the
/// bot's poll loop from receiving later messages or honouring a stop request.
/// Because the update is acked to Telegram as soon as it's enqueued (so the
/// long-poll keeps flowing), the job is also written to an on-disk journal —
/// otherwise a crash mid-download would lose the file with no way to re-fetch
/// it. The journal is replayed on the next start.
#[derive(Clone, Serialize, Deserialize)]
struct PendingDownload {
    update_id: i64,
    file_id: String,
    file_name: Option<String>,
    caption: String,
    chat_id: i64,
    message_id: i64,
}

/// The pending-download journal for one bot, shared between the poll loop (which
/// appends jobs) and the worker (which removes them once saved).
type Journal = Arc<Mutex<Vec<PendingDownload>>>;

/// Path to a bot's pending-download journal, kept beside `status.json`.
fn journal_path(status_path: &Path, id: &str) -> PathBuf {
    let dir = status_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("pending-{}.json", id))
}

/// Load a bot's pending-download journal; an absent or unreadable file is an
/// empty journal.
fn load_journal(path: &Path) -> Vec<PendingDownload> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Delete a bot's pending-download journal — called when the bot is removed so
/// a stale journal can't be replayed against a bot that no longer exists.
pub fn remove_journal(status_path: &Path, id: &str) {
    let _ = std::fs::remove_file(journal_path(status_path, id));
}

/// Persist the journal atomically.
///
/// An empty journal removes the file rather than leaving an empty `[]` behind.
/// Besides keeping things tidy, this closes a race with bot removal: if the
/// worker finishes its last job (emptying the journal) just after `remove_bot`
/// has deleted the file, this removes it again instead of recreating it as an
/// orphan that lingers for a bot that no longer exists.
async fn persist_journal(path: &Path, journal: &Journal) {
    let snapshot = { journal.lock().await.clone() };
    if snapshot.is_empty() {
        let _ = tokio::fs::remove_file(path).await;
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(&snapshot) {
        let _ = atomic_write(path, text.as_bytes());
    }
}

/// Format a byte count for human-readable status/ack messages.
///
/// Uses decimal (SI) units — 1 MB = 1,000,000 bytes — to stay consistent with
/// `PUBLIC_DOWNLOAD_LIMIT` (the 20 MB Bot API cap is decimal too) and with macOS
/// Finder. Binary units here would make a file that's just over the limit read as
/// e.g. "19.1 MB — over the 20 MB limit", which looks self-contradictory.
fn human_size(bytes: i64) -> String {
    if bytes <= 0 {
        return "unknown size".to_string();
    }
    const KB: f64 = 1000.0;
    const MB: f64 = KB * 1000.0;
    const GB: f64 = MB * 1000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// The public Telegram Bot API. Used when a bot has no custom `api_base`.
pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Normalize a configured API base into a usable origin. Falls back to the
/// public API when unset/blank, and trims any trailing slash so URL building
/// can always join with a single `/`.
pub fn resolve_api_base(configured: Option<&str>) -> String {
    let trimmed = configured.map(str::trim).unwrap_or("");
    let base = if trimmed.is_empty() {
        DEFAULT_API_BASE
    } else {
        trimmed
    };
    base.trim_end_matches('/').to_string()
}

/// Call getMe to validate a token and return the bot's @username.
pub async fn get_me(client: &reqwest::Client, base: &str, token: &str) -> Result<String, String> {
    let url = format!("{}/bot{}/getMe", base, token);
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

/// A downloadable attachment pulled from a message.
struct Attachment {
    file_id: String,
    file_name: Option<String>,
    /// Telegram-reported size in bytes, or 0 when the message omits it.
    file_size: i64,
}

/// Pull the first downloadable attachment out of a message, returning its
/// Telegram `file_id`, a suggested filename (when the message provides one) and
/// its reported size. Checks the common attachment kinds in priority order.
fn extract_attachment(msg: &Value) -> Option<Attachment> {
    for key in ["document", "video", "audio", "voice", "animation", "video_note"] {
        if let Some(obj) = msg.get(key) {
            if let Some(id) = obj["file_id"].as_str() {
                return Some(Attachment {
                    file_id: id.to_string(),
                    file_name: obj["file_name"].as_str().map(|s| s.to_string()),
                    file_size: obj["file_size"].as_i64().unwrap_or(0),
                });
            }
        }
    }
    // Photos arrive as an array of sizes; the last entry is the largest.
    if let Some(sizes) = msg["photo"].as_array() {
        if let Some(largest) = sizes.last() {
            if let Some(id) = largest["file_id"].as_str() {
                return Some(Attachment {
                    file_id: id.to_string(),
                    file_name: None,
                    file_size: largest["file_size"].as_i64().unwrap_or(0),
                });
            }
        }
    }
    // Stickers are .webp (or .tgs/.webm for animated) with no file_name.
    if let Some(id) = msg["sticker"]["file_id"].as_str() {
        return Some(Attachment {
            file_id: id.to_string(),
            file_name: None,
            file_size: msg["sticker"]["file_size"].as_i64().unwrap_or(0),
        });
    }
    None
}

/// Resolve where a bot's received/saved files go: the configured `files_dir`,
/// or an `attachments` folder next to the markdown file when unset. Shared by
/// the Telegram download path and the quick-window's local file drops so both
/// land in the same place.
pub fn resolve_save_dir(file: &str, files_dir: Option<&str>) -> PathBuf {
    match files_dir.map(str::trim) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => Path::new(file)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("attachments"))
            .unwrap_or_else(|| PathBuf::from("attachments")),
    }
}

/// Pick a unique destination path inside `dir`, prefixing the name with a
/// timestamp and appending a counter if a same-named file already exists.
pub fn unique_dest(dir: &Path, name: &str) -> PathBuf {
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
    base: &str,
    token: &str,
    file_id: &str,
    file_name: Option<&str>,
    dir: &Path,
) -> Result<PathBuf, DownloadError> {
    // getFile resolves a file_id to a temporary download path on Telegram's servers.
    let get_url = format!("{}/bot{}/getFile", base, token);
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
        .unwrap_or_else(|| {
            file_path
                .rsplit(['/', '\\'])
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("file")
                .to_string()
        });

    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| DownloadError::transient(e.to_string()))?;
    let dest = unique_dest(dir, &name);

    // A self-hosted local Bot API server (run with `--local`) returns `file_path`
    // as an absolute path to the already-downloaded file on disk rather than a
    // relative URL. In that case copy it straight off disk — there's no 20 MB cap
    // and no second HTTP round trip. Otherwise fetch it over HTTP as usual.
    if Path::new(&file_path).is_absolute() {
        tokio::fs::copy(&file_path, &dest)
            .await
            .map_err(|e| DownloadError::permanent(format!("could not read local file: {}", e)))?;
        // The local Bot API server doesn't auto-delete downloaded files — without
        // this its data dir grows unbounded as large videos pile up. We've copied
        // the file out, so the server's copy is safe to remove (best-effort).
        let _ = tokio::fs::remove_file(&file_path).await;
        return Ok(dest);
    }

    let dl_url = format!("{}/file/bot{}/{}", base, token, file_path);
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

    tokio::fs::write(&dest, &bytes)
        .await
        .map_err(|e| DownloadError::transient(e.to_string()))?;
    Ok(dest)
}

async fn react(client: &reqwest::Client, base: &str, token: &str, chat_id: i64, message_id: i64) {
    let url = format!("{}/bot{}/setMessageReaction", base, token);
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

/// Send a plain text message back to the user (e.g. a "downloading…" ack or an
/// error). Best-effort: failures are ignored so they can't stall the loop.
async fn send_message(client: &reqwest::Client, base: &str, token: &str, chat_id: i64, text: &str) {
    let url = format!("{}/bot{}/sendMessage", base, token);
    let body = serde_json::json!({ "chat_id": chat_id, "text": text });
    let _ = client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await;
}

/// Telegram caps a single text message at 4096 UTF-16 code units. We split a
/// little under that; since a string's byte length is always >= its UTF-16
/// length, staying under this many bytes guarantees we're under the real cap.
const TELEGRAM_MSG_LIMIT: usize = 4000;

/// Split text into Telegram-sized chunks, preferring line boundaries so the
/// content stays readable. A single line longer than the limit is hard-split on
/// char boundaries. Concatenating the chunks reproduces the input exactly.
fn chunk_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if line.len() > TELEGRAM_MSG_LIMIT {
            if !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
            }
            let mut rest = line;
            while rest.len() > TELEGRAM_MSG_LIMIT {
                let mut idx = TELEGRAM_MSG_LIMIT;
                while !rest.is_char_boundary(idx) {
                    idx -= 1;
                }
                chunks.push(rest[..idx].to_string());
                rest = &rest[idx..];
            }
            cur.push_str(rest);
            continue;
        }
        if cur.len() + line.len() > TELEGRAM_MSG_LIMIT {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Send one text message, returning whether Telegram accepted it. Unlike
/// `send_message` (fire-and-forget) the daily digest needs to know if a chunk
/// failed so it can surface the error and stop mid-file rather than send a
/// partial digest silently.
async fn send_message_checked(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    chat_id: i64,
    text: &str,
) -> bool {
    let url = format!("{}/bot{}/sendMessage", base, token);
    let body = serde_json::json!({ "chat_id": chat_id, "text": text });
    match client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<Value>()
            .await
            .map(|j| j["ok"].as_bool() == Some(true))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Parse an "HH:MM" string into (hour, minute), clamped to valid ranges.
/// Falls back to 08:00 on anything unparseable.
fn parse_hhmm(s: &str) -> (u32, u32) {
    let mut parts = s.trim().splitn(2, ':');
    let h = parts
        .next()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(8);
    let m = parts
        .next()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(0);
    (h.min(23), m.min(59))
}

/// Once a day at `daily_time` (local), send the markdown file's exact contents
/// back to the owner (`chat_id`). Runs until `stop_rx` flips to true.
///
/// Rather than sleeping for the whole interval (which a laptop sleep or a clock
/// change would throw off), it wakes every 30s, checks the wall clock, and fires
/// once per calendar day when the scheduled minute has arrived. If the bot
/// starts after today's send time, today is treated as already done so there's
/// no surprise catch-up send on launch.
#[allow(clippy::too_many_arguments)]
async fn daily_send_loop(
    id: String,
    base: String,
    token: String,
    file: String,
    chat_id: i64,
    daily_time: String,
    status: StatusMap,
    client: reqwest::Client,
    mut stop_rx: watch::Receiver<bool>,
) {
    use chrono::Timelike;
    let (hh, mm) = parse_hhmm(&daily_time);

    let now = Local::now();
    let past_today = now.hour() > hh || (now.hour() == hh && now.minute() >= mm);
    let mut last_sent: Option<chrono::NaiveDate> = if past_today {
        Some(now.date_naive())
    } else {
        None
    };

    loop {
        tokio::select! {
            // `changed()` errors when the sender is dropped: treat that as a stop
            // too, otherwise the arm would complete instantly every iteration and
            // spin the loop instead of waking once every 30s.
            res = stop_rx.changed() => { if res.is_err() || *stop_rx.borrow() { break; } }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        if *stop_rx.borrow() {
            break;
        }

        let now = Local::now();
        let today = now.date_naive();
        if last_sent == Some(today) {
            continue;
        }
        let due = now.hour() > hh || (now.hour() == hh && now.minute() >= mm);
        if !due {
            continue;
        }

        // Mark before sending so a failure can't spin-resend in a tight loop;
        // it'll simply try again tomorrow.
        last_sent = Some(today);

        match tokio::fs::read_to_string(&file).await {
            Ok(content) => {
                if content.trim().is_empty() {
                    // An empty file isn't a failure — today's send is a no-op, so
                    // clear any stale error from a previous day rather than letting
                    // it linger until the next non-empty send.
                    set_status(&status, &id, |s| s.last_daily_error = None).await;
                    continue; // nothing to send today
                }
                let mut ok = true;
                for chunk in chunk_text(&content) {
                    if !send_message_checked(&client, &base, &token, chat_id, &chunk).await {
                        ok = false;
                        break;
                    }
                }
                set_status(&status, &id, |s| {
                    s.last_daily_error = if ok {
                        None
                    } else {
                        Some("daily send failed (Telegram rejected message)".to_string())
                    };
                })
                .await;
            }
            Err(e) => {
                set_status(&status, &id, |s| {
                    s.last_daily_error = Some(format!("daily send: cannot read file: {}", e));
                })
                .await;
            }
        }
    }
}

/// Background worker that drains a bot's download queue. Runs one download at a
/// time (preserving order), retrying transient failures until they succeed, the
/// failure turns out permanent, or the bot stops. Each finished or permanently
/// failed job is removed from the journal so it isn't replayed next start.
#[allow(clippy::too_many_arguments)]
async fn download_worker(
    id: String,
    base: String,
    token: String,
    file: String,
    save_dir: PathBuf,
    status: StatusMap,
    status_path: PathBuf,
    journal: Journal,
    journal_file: PathBuf,
    client: reqwest::Client,
    mut rx: mpsc::UnboundedReceiver<PendingDownload>,
    mut stop_rx: watch::Receiver<bool>,
) {
    loop {
        let job = tokio::select! {
            // `changed()` errors when the sender is dropped; treat that as a stop
            // too (matching `daily_send_loop`). We only ever send `true`, so any
            // change means stop. Without the `is_err()` check a dropped sender
            // would make this arm complete instantly every iteration and spin the
            // loop instead of ending the worker.
            res = stop_rx.changed() => {
                if res.is_err() || *stop_rx.borrow() { break; } else { continue; }
            }
            j = rx.recv() => match j {
                Some(j) => j,
                None => break, // sender dropped: bot is shutting down
            },
        };

        // A duplicate (e.g. replayed from the journal *and* re-delivered by
        // Telegram after a crash) may already be gone — skip if so.
        if !journal.lock().await.iter().any(|p| p.update_id == job.update_id) {
            continue;
        }

        // Retry transient failures with backoff; stop promptly if asked.
        loop {
            if *stop_rx.borrow() {
                return; // leave the job in the journal for the next start
            }
            match download_attachment(
                &client,
                &base,
                &token,
                &job.file_id,
                job.file_name.as_deref(),
                &save_dir,
            )
            .await
            {
                Ok(dest) => {
                    let name = dest
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".to_string());
                    // The size comes from the saved file itself, so it's accurate
                    // even when Telegram didn't report one.
                    let size = tokio::fs::metadata(&dest)
                        .await
                        .map(|m| m.len() as i64)
                        .unwrap_or(0);
                    let mut note = format!("saved file: {} → {}", name, dest.display());
                    if !job.caption.is_empty() {
                        note.push_str(&format!(" — {}", job.caption));
                    }
                    match append_timestamped(&file, &note).await {
                        Ok(()) => {
                            set_status(&status, &id, |s| {
                                s.message_count += 1;
                                s.last_message_at =
                                    Some(Local::now().format("%H:%M").to_string());
                                s.last_error = None;
                            })
                            .await;
                            persist_status(&status_path, &status).await;
                            react(&client, &base, &token, job.chat_id, job.message_id).await;
                            // Confirm completion. The ack is sent only once the file
                            // is actually on disk (there's no "Saving…" message
                            // beforehand), so a slow, large download no longer looks
                            // stuck mid-save.
                            send_message(
                                &client,
                                &base,
                                &token,
                                job.chat_id,
                                &format!("✅ Saved {} ({})", name, human_size(size)),
                            )
                            .await;
                        }
                        Err(e) => {
                            // The file downloaded fine but its note couldn't be
                            // written. Record the error and say so, rather than
                            // sending a misleading "✅ Saved" with no record of it
                            // in the markdown file. The file is on disk, so the job
                            // is still considered done (retrying would re-download).
                            set_status(&status, &id, |s| {
                                s.last_error =
                                    Some(format!("file saved but note write failed: {}", e));
                            })
                            .await;
                            send_message(
                                &client,
                                &base,
                                &token,
                                job.chat_id,
                                &format!(
                                    "⚠️ Downloaded {} ({}) but couldn't write its note to the file: {}",
                                    name,
                                    human_size(size),
                                    e
                                ),
                            )
                            .await;
                        }
                    }
                    break;
                }
                Err(e) if e.permanent => {
                    let mut note = format!("could not save file: {}", e.msg);
                    if !job.caption.is_empty() {
                        note.push_str(&format!(" — {}", job.caption));
                    }
                    let _ = append_timestamped(&file, &note).await;
                    send_message(&client, &base, &token, job.chat_id, &format!("⚠️ {}", e.msg))
                        .await;
                    break;
                }
                Err(e) => {
                    set_status(&status, &id, |s| {
                        s.last_error = Some(format!("file save failed: {}", e.msg));
                    })
                    .await;
                    // Back off, but wake immediately on stop so shutdown isn't delayed.
                    tokio::select! {
                        _ = stop_rx.changed() => { if *stop_rx.borrow() { return; } }
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    }
                    continue;
                }
            }
        }

        // Done (saved or permanently failed): drop it from the journal.
        journal.lock().await.retain(|p| p.update_id != job.update_id);
        persist_journal(&journal_file, &journal).await;
    }
}

/// Long-poll loop for a single bot. Runs until `stop_rx` flips to true.
#[allow(clippy::too_many_arguments)]
pub async fn run_bot(
    id: String,
    token: String,
    file: String,
    files_dir: Option<String>,
    allowed_user_id: i64,
    api_base: Option<String>,
    daily_send: bool,
    daily_time: String,
    status: StatusMap,
    status_path: PathBuf,
    client: reqwest::Client,
    mut stop_rx: watch::Receiver<bool>,
) {
    let base = resolve_api_base(api_base.as_deref());

    // If a daily digest is configured, spawn it alongside the poll loop. It needs
    // a concrete chat to send to: in a private chat the owner's user id is also
    // the chat id, so we require `allowed_user_id` to be set.
    if daily_send && allowed_user_id != 0 {
        tokio::spawn(daily_send_loop(
            id.clone(),
            base.clone(),
            token.clone(),
            file.clone(),
            allowed_user_id,
            daily_time,
            status.clone(),
            client.clone(),
            stop_rx.clone(),
        ));
    } else {
        // The daily digest isn't active for this (re)start. Clear any stale daily
        // error left in memory from a previous run so the tray and UI don't keep
        // showing a red error for a send that's no longer scheduled (e.g. after
        // the user unticks daily-send following a failed send). `last_daily_error`
        // is otherwise only ever cleared by a *successful* send, which can't
        // happen once the loop is gone.
        set_status(&status, &id, |s| s.last_daily_error = None).await;
    }

    // Validate token / fetch username up front.
    match get_me(&client, &base, &token).await {
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
    let updates_url = format!("{}/bot{}/getUpdates", base, token);

    // Resolve where received files are saved: the configured folder, or an
    // `attachments` folder next to the markdown file when unset.
    let save_dir: PathBuf = resolve_save_dir(&file, files_dir.as_deref());

    // Whether file downloads are subject to the public API's 20 MB cap.
    let capped = base == DEFAULT_API_BASE;

    // Per-bot download queue + crash-safe journal. Files are saved on a
    // background worker so a slow download can't stall the poll loop; the
    // journal lets a restart resume downloads that were acked to Telegram but
    // not yet written to disk.
    let journal_file = journal_path(&status_path, &id);
    let journal: Journal = Arc::new(Mutex::new(load_journal(&journal_file)));
    let (tx, rx) = mpsc::unbounded_channel::<PendingDownload>();
    // Re-enqueue anything left over from a previous run.
    for job in journal.lock().await.iter().cloned() {
        let _ = tx.send(job);
    }
    tokio::spawn(download_worker(
        id.clone(),
        base.clone(),
        token.clone(),
        file.clone(),
        save_dir.clone(),
        status.clone(),
        status_path.clone(),
        journal.clone(),
        journal_file.clone(),
        client.clone(),
        rx,
        stop_rx.clone(),
    ));

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

                            // Decide what to do. A file/photo/etc is queued for a
                            // background download (so a slow transfer can't stall this
                            // loop); a plain text message is appended inline; anything
                            // else is acknowledged but not saved.
                            let caption = msg["caption"].as_str().unwrap_or("").trim().to_string();

                            if let Some(att) = extract_attachment(msg) {
                                let Some(uid) = update_id else { continue };
                                let display_name =
                                    att.file_name.clone().unwrap_or_else(|| "file".to_string());

                                // Already queued (re-delivered after a crash, or replayed
                                // from the journal) — just confirm it and move on.
                                if journal.lock().await.iter().any(|p| p.update_id == uid) {
                                    offset = offset.max(uid + 1);
                                    continue;
                                }

                                // Pre-empt the public API's 20 MB cap: a getFile we know
                                // will fail is worth skipping. Record the note now and tell
                                // the user why instead of attempting a doomed download.
                                if capped
                                    && att.file_size > PUBLIC_DOWNLOAD_LIMIT
                                {
                                    let mut note = format!(
                                        "could not save file: {} is {} — over the 20 MB Bot API limit; enable the local server to receive it",
                                        display_name,
                                        human_size(att.file_size)
                                    );
                                    if !caption.is_empty() {
                                        note.push_str(&format!(" — {}", caption));
                                    }
                                    match append_timestamped(&file, &note).await {
                                        Ok(()) => {
                                            offset = offset.max(uid + 1);
                                            // The file was *not* saved, so don't bump
                                            // the "saved" counter — just record the
                                            // activity timestamp for the dashboard.
                                            set_status(&status, &id, |s| {
                                                s.last_message_at =
                                                    Some(Local::now().format("%H:%M").to_string());
                                            })
                                            .await;
                                            send_message(
                                                &client,
                                                &base,
                                                &token,
                                                chat_id,
                                                &format!(
                                                    "⚠️ {} ({}) is over the 20 MB limit and was not saved.",
                                                    display_name,
                                                    human_size(att.file_size)
                                                ),
                                            )
                                            .await;
                                        }
                                        Err(e) => {
                                            set_status(&status, &id, |s| {
                                                s.last_error = Some(format!("write failed: {}", e));
                                            })
                                            .await;
                                            write_failed = true;
                                            break;
                                        }
                                    }
                                    continue;
                                }

                                // Journal the job *before* advancing the offset so a crash
                                // can't ack the update to Telegram with no record of it,
                                // then hand it to the background worker.
                                let job = PendingDownload {
                                    update_id: uid,
                                    file_id: att.file_id,
                                    file_name: att.file_name,
                                    caption: caption.clone(),
                                    chat_id,
                                    message_id,
                                };
                                journal.lock().await.push(job.clone());
                                persist_journal(&journal_file, &journal).await;
                                let _ = tx.send(job);
                                offset = offset.max(uid + 1);
                                // No "Saving…" ack here on purpose: the worker sends
                                // a single "✅ Saved …" message once the download
                                // actually finishes, so a long transfer doesn't look
                                // perpetually stuck on a "Saving…" note.
                                continue;
                            }

                            // Plain text: append inline (fast).
                            let Some(text) = msg["text"].as_str().map(|s| s.to_string()) else {
                                // Non-text, non-file update: nothing to save, but confirm
                                // it so Telegram doesn't keep re-delivering it.
                                if let Some(uid) = update_id {
                                    offset = offset.max(uid + 1);
                                }
                                continue;
                            };

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
                                    react(&client, &base, &token, chat_id, message_id).await;
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
