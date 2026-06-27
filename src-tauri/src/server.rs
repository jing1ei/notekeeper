//! App-managed local Telegram Bot API server.
//!
//! The public Bot API at api.telegram.org caps file downloads at 20 MB, which
//! is smaller than most videos. A self-hosted `telegram-bot-api` server run in
//! `--local` mode lifts that to 2 GB and writes downloaded files straight to
//! disk, so the app can read them without a second HTTP round trip.
//!
//! This module locates the binary and supervises a single child process. The
//! process is killed when its [`ServerHandle`] is dropped, so it never outlives
//! the app.

use crate::config::ServerConfig;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Install locations probed when no explicit binary path is configured.
/// Covers Homebrew on Apple Silicon and Intel plus the usual system prefixes.
const CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/telegram-bot-api",
    "/usr/local/bin/telegram-bot-api",
    "/usr/bin/telegram-bot-api",
];

/// Resolve the `telegram-bot-api` binary: an explicit configured path first,
/// then the common install locations, then a `PATH` lookup via `which`.
/// Returns `None` if nothing usable is found.
pub fn locate_binary(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        let pb = PathBuf::from(p);
        return pb.exists().then_some(pb);
    }
    for c in CANDIDATES {
        let pb = PathBuf::from(c);
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Ok(out) = Command::new("which").arg("telegram-bot-api").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(PathBuf::from(s));
            }
        }
    }
    None
}

/// The origin a bot should talk to when the local server is running.
pub fn local_url(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// A running local Bot API server. Dropping the handle kills the child process
/// (and reaps it), so the server can't be left orphaned when the app exits or
/// the settings change.
pub struct ServerHandle {
    child: Child,
    pub port: u16,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the local `telegram-bot-api` server in `--local` mode.
///
/// `data_dir` is where the server stores downloaded files; the app reads them
/// from there directly (see `bots::download_attachment`). Fails fast with a
/// human-readable message when credentials are missing or the binary can't be
/// found, so the error can be surfaced in the settings UI.
pub fn start(cfg: &ServerConfig, data_dir: &Path) -> Result<ServerHandle, String> {
    if cfg.api_id == 0 || cfg.api_hash.trim().is_empty() {
        return Err("api_id and api_hash are required to run the local server".into());
    }
    let bin = locate_binary(cfg.bin_path.as_deref()).ok_or_else(|| {
        "telegram-bot-api not found — install it (e.g. `brew install telegram-bot-api`) \
         or set its path in server settings"
            .to_string()
    })?;

    let temp_dir = data_dir.join("temp");
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    let port = cfg.effective_port();

    // Bind only to loopback so the server isn't reachable from other machines.
    let child = Command::new(&bin)
        .arg("--local")
        .arg("--http-ip-address=127.0.0.1")
        .arg(format!("--http-port={}", port))
        .arg(format!("--api-id={}", cfg.api_id))
        .arg(format!("--api-hash={}", cfg.api_hash))
        .arg(format!("--dir={}", data_dir.display()))
        .arg(format!("--temp-dir={}", temp_dir.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start telegram-bot-api: {}", e))?;

    Ok(ServerHandle { child, port })
}
