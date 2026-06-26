//! Bot-token storage backed by the macOS Keychain.
//!
//! Tokens are kept out of `bots.json` entirely: the config file stores only the
//! non-secret bot metadata, while each token lives in the login Keychain under
//! the service `com.notekeeper.app`, keyed by the bot's UUID. This keeps secrets
//! out of plaintext on disk (and out of any backup of the config file).

use crate::config::Config;
use keyring::Entry;

/// Keychain service name. Matches the app's bundle identifier.
const SERVICE: &str = "com.notekeeper.app";

/// Read a bot's token from the Keychain. Returns `None` if there is no entry
/// (or the Keychain is unavailable).
pub fn get_token(id: &str) -> Option<String> {
    Entry::new(SERVICE, id).ok()?.get_password().ok()
}

/// Store (or replace) a bot's token in the Keychain.
pub fn set_token(id: &str, token: &str) -> Result<(), String> {
    Entry::new(SERVICE, id)
        .and_then(|e| e.set_password(token))
        .map_err(|e| e.to_string())
}

/// Delete a bot's token from the Keychain. Missing entries are ignored.
pub fn delete_token(id: &str) {
    if let Ok(e) = Entry::new(SERVICE, id) {
        let _ = e.delete_credential();
    }
}

/// Fill each bot's in-memory token from the Keychain.
///
/// If a bot has no Keychain entry but still carries a token in memory (i.e. it
/// was just loaded from an older plaintext `bots.json`), the token is migrated
/// into the Keychain. Returns `true` if any token was migrated, signalling the
/// caller to re-save the config so the cleartext token is removed from disk.
pub fn hydrate_tokens(config: &mut Config) -> bool {
    let mut migrated = false;
    for b in &mut config.bots {
        match get_token(&b.id) {
            Some(tok) => b.token = tok,
            None => {
                if !b.token.is_empty() && set_token(&b.id, &b.token).is_ok() {
                    migrated = true;
                }
            }
        }
    }
    migrated
}
