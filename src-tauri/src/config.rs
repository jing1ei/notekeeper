use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

fn default_true() -> bool {
    true
}

fn default_port() -> u16 {
    8081
}

fn default_daily_time() -> String {
    "08:00".to_string()
}

/// Settings for an app-managed local Telegram Bot API server. Running one lifts
/// the public API's 20 MB download cap to 2 GB, so large videos can be saved.
/// A single server instance serves every bot.
#[derive(Serialize, Deserialize, Clone)]
pub struct ServerConfig {
    /// Whether the app should spawn and manage a local server.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the `telegram-bot-api` binary. Unset/empty = auto-detect from
    /// common install locations and `PATH`.
    #[serde(default)]
    pub bin_path: Option<String>,
    /// HTTP port the local server listens on.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Telegram `api_id` from my.telegram.org. Not secret.
    #[serde(default)]
    pub api_id: i64,
    /// Telegram `api_hash`. Secret: stored in the Keychain, never serialized to
    /// `bots.json`. Hydrated into memory at startup.
    #[serde(default, skip_serializing)]
    pub api_hash: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bin_path: None,
            port: default_port(),
            api_id: 0,
            api_hash: String::new(),
        }
    }
}

impl ServerConfig {
    /// The port the server actually listens on. A `0` (e.g. a hand-edited
    /// config) falls back to the default so the spawned server and the URL bots
    /// are routed to can never disagree.
    pub fn effective_port(&self) -> u16 {
        if self.port == 0 {
            default_port()
        } else {
            self.port
        }
    }
}

/// One bot bound to one markdown file.
#[derive(Serialize, Deserialize, Clone)]
pub struct BotConfig {
    pub id: String,
    pub name: String,
    /// The Telegram bot token. In-memory only: stored in the macOS Keychain (see
    /// `secrets.rs`), never serialized to `bots.json`. `default` lets older
    /// plaintext configs still deserialize so their token can be migrated out.
    #[serde(default, skip_serializing)]
    pub token: String,
    pub file: String,
    /// Folder where received files (documents, photos, etc.) are saved. If unset
    /// or empty, files go to an `attachments` folder next to the markdown file.
    #[serde(default)]
    pub files_dir: Option<String>,
    /// Telegram numeric user id allowed to write. 0 = allow anyone (not recommended).
    #[serde(default)]
    pub allowed_user_id: i64,
    /// Base URL of the Telegram Bot API. Unset/empty uses the public
    /// `https://api.telegram.org`, which caps file downloads at 20 MB. Point this
    /// at a self-hosted local Bot API server (e.g. `http://127.0.0.1:8081` — the
    /// app binds the managed server to `127.0.0.1`, so prefer that over
    /// `localhost`, which can resolve to IPv6 `::1` and miss it) to
    /// raise the limit to 2 GB so large videos can be saved.
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional global hotkey for local quick-capture, e.g. "CmdOrCtrl+Shift+KeyI".
    #[serde(default)]
    pub shortcut: Option<String>,
    /// When true, the bot sends the markdown file's exact contents back to the
    /// owner once a day at `daily_time`. Requires `allowed_user_id` to be set (in
    /// a private chat the user id is also the chat id we send to).
    #[serde(default)]
    pub daily_send: bool,
    /// Local time of day for the daily send, "HH:MM" (24-hour). Defaults to 08:00.
    #[serde(default = "default_daily_time")]
    pub daily_time: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub bots: Vec<BotConfig>,
    /// Global settings for the app-managed local Bot API server.
    #[serde(default)]
    pub server: ServerConfig,
}

impl Config {
    /// Load config from disk. Returns an empty config if the file does not exist.
    ///
    /// If the file exists but cannot be parsed, the corrupt file is moved aside
    /// (to `<name>.corrupt-<timestamp>`) rather than silently discarded, so a
    /// subsequent `save()` can never overwrite recoverable data.
    pub fn load(path: &Path) -> Config {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Config::default(),
        };
        match serde_json::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                let backup = path.with_extension(format!(
                    "corrupt-{}",
                    chrono::Local::now().format("%Y%m%d-%H%M%S")
                ));
                let _ = std::fs::rename(path, &backup);
                eprintln!(
                    "notekeeper: failed to parse {} ({e}); moved aside to {}",
                    path.display(),
                    backup.display()
                );
                Config::default()
            }
        }
    }

    /// Persist config to disk (pretty JSON), written atomically.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(path, text.as_bytes())
    }
}

/// Write `bytes` to `path` atomically: write to a sibling temp file, fsync, then
/// rename over the target. A crash mid-write leaves the original file intact.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Unique temp name so concurrent writers (e.g. several bots persisting
    // status at once) don't clobber each other's in-flight temp file.
    let tmp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
