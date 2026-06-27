# Notekeeper

A macOS **menu bar app**. Each Telegram bot is bound 1-to-1 to a local markdown file.
Send the bot any text message and it appends that text (with a timestamp) to its file.
Send it a file, photo, or other attachment and it saves the file to a folder you choose
and adds a timestamped note to the markdown file recording what was saved and where.
Manage the bindings — add, edit, enable/disable, remove — from a small table UI, and
watch each bot's live status from the menu bar.

```
You ──Telegram──▶ Bot "Ideas"  ──▶ /Users/you/notes/ideas.md
You ──Telegram──▶ Bot "Tasks"  ──▶ /Users/you/notes/tasks.md
```

The app uses **long polling**, so there is no inbound server, no public port, and no
domain to configure — it just dials out to Telegram. Perfect for a Mac mini at home.
(Optionally it can run a *local* Telegram Bot API server bound to loopback to lift the
20 MB download cap — see "Local Bot API server" below.)

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

The icon set in `src-tauri/icons/` is already committed, so you can skip this. Only
regenerate it if you swap in your own 1024×1024 PNG:

```bash
cargo tauri icon app-icon.png
```

## 2. Create your bots in Telegram

For **each** markdown file you want to feed:

1. In Telegram, open a chat with **@BotFather** → `/newbot` → follow the prompts.
2. Copy the **token** it gives you (looks like `123456:ABC-DEF...`).
3. Open a chat with **@userinfobot** once and note **your** numeric user ID — this is the
   only account allowed to write to your files.

## 3. Run it

The repo ships three double-clickable helper scripts — run them from Finder, no
Terminal needed. The two build scripts set up cargo's PATH and add resilience for
flaky networks:

- **`run-dev.command`** — run the app from the current source with the dev console
  open (no install, no stale `/Applications` copy). Good for a first try.
- **`build.command`** — compile a release bundle and reveal `Notekeeper.app` in Finder.
- **`commit.command`** — stage everything, commit (prompts for a message), and push.

Prefer the command line? The equivalents are:

```bash
cargo tauri dev     # development / first try
cargo tauri build   # build a real app bundle
```

The finished app is at
`src-tauri/target/release/bundle/macos/Notekeeper.app`
(and usually a `.dmg` under `bundle/dmg/` — the `.dmg` is just a convenience and
is skipped if macOS won't let Terminal control Finder; the `.app` is all you
need). Drag `Notekeeper.app` into `/Applications`.

When it launches there's **no dock icon** — look for the Notekeeper icon in the **menu
bar**. Click it → **Open Manager** to add bots.

## 4. Add a bot in the app

Click **+ Add bot**, then:

- **Name** — anything, e.g. "Ideas"
- **Bot token** — paste it, click **Validate token** to confirm it connects
- **Markdown file** — **Browse…** to pick an existing file, or type a full path (it's
  created automatically on the first message if it doesn't exist)
- **Files folder** *(optional)* — **Browse…** to pick where received files (documents,
  photos, etc.) are saved. Leave blank to use an `attachments` folder next to the
  markdown file.
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
format as the bot); **Esc** or clicking away dismisses it. You can also **drag files onto
the box** to copy them into the bot's files folder (the same place Telegram attachments go),
with a note added to the markdown file — exactly like sending the bot a file.

Set one per bot in the manager: edit the bot, click **Record**, and press the keys (e.g.
⌘⇧I). Pick combinations that include ⌘/⌥/⌃ so they don't clash with normal typing. The
hotkey works whether or not the bot's Telegram side is enabled.

> macOS will ask for **Accessibility/Input Monitoring** permission the first time, so it
> can listen for global hotkeys — approve Notekeeper in
> System Settings → Privacy & Security.

## Daily send-back (file → you, every morning)

Each bot can also push **the other way**: tick **Send this file to me every day at**
in the bot's edit form and pick a time. Once a day at that local time the bot sends
the markdown file's **exact contents** back to you in Telegram (split across multiple
messages if it's longer than Telegram's per-message limit).

- It sends to the account in **Your Telegram user ID**, so that field must be set
  (it's also the chat the bot replies in). Leaving it at `0` disables the daily send.
- Only sends when the bot is **enabled** — it rides along with the poll loop.
- If the file is empty (or missing) that day, nothing is sent.
- Starting the app *after* the scheduled time won't trigger a catch-up send; it waits
  until the next day's slot.

## How messages are written

Each message becomes one timestamped line, appended to the file:

```
- [2026-06-25 14:30] pick up dry cleaning
- [2026-06-25 14:31] idea: a bot that writes to markdown
```

Send a file, photo, or other attachment and it's downloaded to the bot's **Files folder**;
a note recording the saved filename and path (plus any caption) is appended to the file:

```
- [2026-06-25 14:32] saved file: 20260625-143200_report.pdf → /Users/you/notes/attachments/20260625-143200_report.pdf
- [2026-06-25 14:33] saved file: 20260625-143300_photo.jpg → /Users/you/notes/attachments/20260625-143300_photo.jpg — vacation pic
```

Saved files are prefixed with a timestamp so names never collide. Only messages from your
allowed user ID are saved; anything else is ignored. The bot reacts with 👍 on each saved
message or file so you get confirmation in Telegram.

Only text and downloadable attachments (documents, photos, videos, audio, voice, stickers,
etc.) are saved. Other message types — locations, contacts, polls, and the like — are
acknowledged (so Telegram stops re-delivering them) but nothing is written to the file. For files, it also replies with a
`✅ Saved <name> (<size>)` message once the download actually finishes — large transfers
confirm on completion rather than up front, so they never look stuck mid-save.

## Where settings live

Your bot list (names, file paths, user IDs, shortcuts) is stored at:

```
~/Library/Application Support/com.notekeeper.desktop/bots.json
```

**Bot tokens are not kept in that file.** They live in the macOS **Keychain**
(service `com.notekeeper.app`, one entry per bot). If you're upgrading from an
older build that stored tokens inline in `bots.json`, they're migrated into the
Keychain automatically on first launch and stripped from the file.

## Project layout

```
notekeeper/
├─ app-icon.png              # source icon (run: cargo tauri icon app-icon.png)
├─ run-dev.command           # double-click: run from source (dev console)
├─ build.command             # double-click: build Notekeeper.app
├─ commit.command            # double-click: stage, commit & push
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
      ├─ server.rs           # optional app-managed local Bot API server
      ├─ secrets.rs          # bot tokens in the macOS Keychain
      └─ config.rs           # load/save bots.json (no tokens)
```

## Notes & limits

- A bot token can only run **one** long-poll consumer at a time. Don't run the same
  token elsewhere (e.g. a second copy of the app) or Telegram returns a 409 conflict.
- Leaving the user ID at `0` lets **anyone** who finds the bot write to your file — set
  your real ID unless you have a reason not to.
- Telegram's **public** Bot API caps file downloads at ~20 MB. Over the public API a
  larger file can't be fetched; instead of saving it, the bot appends a
  `could not save file:` note and moves on. Run the optional **local Bot API server**
  (below) to raise this to 2 GB.

## Local Bot API server (optional, lifts the 20 MB cap)

To receive files larger than 20 MB (most videos), the app can spawn and manage a local
[`telegram-bot-api`](https://github.com/tdlib/telegram-bot-api) server in `--local` mode,
which raises the download limit to 2 GB and writes files straight to disk.

1. Build `telegram-bot-api` from source once — there is **no Homebrew formula** for it.
   The easiest path is the official build-instructions generator at
   <https://tdlib.github.io/telegram-bot-api/build.html> (pick macOS). In short:

   ```bash
   xcode-select --install
   brew install gperf cmake openssl zlib
   git clone --recursive https://github.com/tdlib/telegram-bot-api.git
   cd telegram-bot-api && mkdir build && cd build
   cmake -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX:PATH=.. \
         -DOPENSSL_ROOT_DIR=$(brew --prefix openssl) ..
   cmake --build . --target install
   ```

   The binary lands at `telegram-bot-api/bin/telegram-bot-api`. Either copy it into
   `/opt/homebrew/bin` (Apple Silicon) or `/usr/local/bin` (Intel) so the app
   auto-detects it, or paste its full path into the **Server binary path** field.
2. Get an `api_id` and `api_hash` from **my.telegram.org → API development tools**.
3. In the app, click **Local server**, tick "Run the local server…", enter the
   `api_id`/`api_hash`, and save. The `api_hash` is stored in the macOS Keychain.

The server binds to **loopback only** (`127.0.0.1`), so it isn't reachable from other
machines, and it's killed when the app quits. All enabled bots are routed through it
while it's running; if it's enabled but fails to start, the failure is shown in the
**Local server** dialog and bots fall back to the public API automatically.

> A bot already used with the public API must be logged out of it before a local server
> can take over — send `https://api.telegram.org/bot<token>/logOut` once per bot. It logs
> back in automatically if you later disable the local server.
