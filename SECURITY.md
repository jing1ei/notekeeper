# Security Policy

## Reporting a vulnerability

If you find a security issue in Notekeeper, please report it privately rather
than opening a public issue.

- Use GitHub's **"Report a vulnerability"** button under the repository's
  **Security** tab (Private Vulnerability Reporting), or
- Open a regular issue **only** for non-sensitive, low-risk reports.

Please include steps to reproduce, the affected version/commit, and the impact
you observed. You'll get an acknowledgement as soon as practical, and a fix or
mitigation will be coordinated before any public disclosure.

## Scope and threat model

Notekeeper is a local, single-user macOS menu bar app. It does **not** run a
server or open any inbound ports — it only dials out to Telegram over HTTPS via
long polling. The main assets to protect are:

- **Bot tokens** — stored in the macOS **Keychain** (service
  `com.notekeeper.app`), not in `bots.json`. Tokens are redacted from any error
  messages shown in the UI, tray, or logs.
- **Your notes** — plain markdown/text files at paths you choose, plus any
  files/photos received (saved to a folder you pick). Only messages from the
  configured Telegram user ID are written or downloaded.

### Things to be aware of

- **`allowed_user_id` gating.** Each bot only saves messages from the numeric
  Telegram user ID you set. Leaving it at `0` allows **anyone** who discovers
  the bot to write to your file. Always set your real ID.
- **One poller per token.** Running the same token in two places triggers
  Telegram's 409 conflict. Don't share a token across instances.
- **Local trust.** The UI is static, fully local content. Dynamic values
  (bot names, file paths, errors, Telegram usernames) are HTML-escaped before
  display, and note content is never rendered in the app — only written to disk.

## Supported versions

This is a small project; security fixes are applied to the latest release and
`main` only.
