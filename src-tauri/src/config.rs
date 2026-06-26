use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

fn default_true() -> bool {
    true
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
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional global hotkey for local quick-capture, e.g. "CmdOrCtrl+Shift+KeyI".
    #[serde(default)]
    pub shortcut: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub bots: Vec<BotConfig>,
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
