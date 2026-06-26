# Notekeeper

A macOS **menu bar app**. Each Telegram bot is bound 1-to-1 to a local markdown file.
Send the bot any text message and it appends that text (with a timestamp) to its file.
Manage the bindings — add, edit, enable/disable, remove — from a small table UI, and
watch each bot's live status from the menu bar.

```
You ──Telegram──▶ Bot "Ideas"  ──▶ /Users/you/notes/ideas.md
You ──Telegram──▶ Bot "Tasks"  ──▶ /Users/you/notes/tasks.md
```

The app uses **long polling**, so there is no server, no open port, and no domain to
configure — it just dials out to Telegram. Perfect for a Mac mini at home.

---

## 1. One-time setup on the Mac mini

Install the toolchain (only needed to build the app):

```bash
# Xcode command line tools
xcode-select --install

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Tauri CLI (used to build/run)
cargo install tauri-cli --version "^2"
```

Generate the icon set from the supplied source image (run once, from the project root):

```bash
cargo tauri icon app-icon.png
```

(That creates `src-tauri/icons/`. Replace `app-icon.png` with your own 1024×1024 PNG
first if you'd like a different icon.)

## 2. Create your bots in Telegram

For **each** markdown file you want to feed:

1. In Telegram, open a chat with **@BotFather** → `/newbot` → follow the prompts.
2. Copy the **token** it gives you (looks like `123456:ABC-DEF...`).
3. Open a chat with **@userinfobot** once and note **your** numeric user ID — this is the
   only account allowed to write to your files.

## 3. Run it

Development / first try:

```bash
cargo tauri dev
```

Build a real app bundle:

```bash
cargo tauri build
```

The finished app is at
`src-tauri/target/release/bundle/macos/Notekeeper.app`
(and a `.dmg` under `bundle/dmg/`). Drag `Notekeeper.app` into `/Applications`.

When it launches there's **no dock icon** — look for the Notekeeper icon in the **menu
bar**. Click it → **Open Manager** to add bots.

## 4. Add a bot in the app

Click **+ Add bot**, then:

- **Name** — anything, e.g. "Ideas"
- **Bot token** — paste it, click **Validate token** to confirm it connects
- **Markdown file** — **Browse…** to pick an existing file, or type a full path (it's
  created automatically on the first message if it doesn't exist)
- **Your Telegram user ID** — from @userinfobot
- **Local shortcut** *(optional)* — click **Record** and press a hotkey (see below)
- **Enabled** — leave checked to start immediately

The menu bar tooltip and dropdown show each bot's status: 🟢 running, 🔴 error,
🟡 starting, ⚪ disabled, plus a count of messages saved.

In the manager table, click a bot's **markdown file path** to open it in the default
text editor (it's created first if it doesn't exist yet) so you can read or edit notes.

## 5. Keep it running 24/7

Because it's a menu bar app, it runs as long as you're logged in:

1. **System Settings → General → Login Items** → add **Notekeeper** so it starts on boot.
2. **System Settings → Users & Groups → Automatically log in as** → set to your account,
   so a reboot lands you logged in (required for menu bar apps to run unattended).

Each bot also auto-reconnects after network blips on its own.

---

## Local quick-capture (no Telegram needed)

Each file can have a **global hotkey**. Press it anywhere on the Mac — even when
Notekeeper isn't focused — and a small input box pops up over whatever you're doing,
already pointed at that file. Type, press **Enter**, and it's appended (same timestamped
format as the bot); **Esc** or clicking away dismisses it.

Set one per bot in the manager: edit the bot, click **Record**, and press the keys (e.g.
⌘⇧I). Pick combinations that include ⌘/⌥/⌃ so they don't clash with normal typing. The
hotkey works whether or not the bot's Telegram side is enabled.

> macOS will ask for **Accessibility/Input Monitoring** permission the first time, so it
> can listen for global hotkeys — approve Notekeeper in
> System Settings → Privacy & Security.

## How messages are written

Each message becomes one timestamped line, appended to the file:

```
- [2026-06-25 14:30] pick up dry cleaning
- [2026-06-25 14:31] idea: a bot that writes to markdown
```

Only text messages from your allowed user ID are saved; everything else is ignored.
The bot reacts with 👍 on each saved message so you get confirmation in Telegram.

## Where settings live

Your bot list (names, file paths, user IDs, shortcuts) is stored at:

```
~/Library/Application Support/com.notekeeper.app/bots.json
```

**Bot tokens are not kept in that file.** They live in the macOS **Keychain**
(service `com.notekeeper.app`, one entry per bot). If you're upgrading from an
older build that stored tokens inline in `bots.json`, they're migrated into the
Keychain automatically on first launch and stripped from the file.

## Project layout

```
notekeeper/
├─ app-icon.png              # source icon (run: cargo tauri icon app-icon.png)
├─ src/                      # the UI (static HTML/CSS/JS — no build step)
│  ├─ index.html             # bot manager table
│  └─ quick.html             # the pop-up quick-capture box
└─ src-tauri/                # Rust backend
   ├─ Cargo.toml
   ├─ tauri.conf.json
   ├─ build.rs
   ├─ capabilities/default.json
   └─ src/
      ├─ main.rs             # Tauri commands + tray + lifecycle
      ├─ bots.rs             # Telegram long-poll engine, timestamped append
      ├─ secrets.rs          # bot tokens in the macOS Keychain
      └─ config.rs           # load/save bots.json (no tokens)
```

## Notes & limits

- A bot token can only run **one** long-poll consumer at a time. Don't run the same
  token elsewhere (e.g. a second copy of the app) or Telegram returns a 409 conflict.
- Leaving the user ID at `0` lets **anyone** who finds the bot write to your file — set
  your real ID unless you have a reason not to.
