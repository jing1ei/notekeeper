#!/bin/bash
# Double-click this file in Finder to build Notekeeper.app locally.
# It compiles a release bundle and reveals the result in Finder.

# Always run from the folder this script lives in (the project root).
cd "$(dirname "$0")" || exit 1

# A double-clicked script doesn't inherit your shell PATH, so add the usual
# spots where rustup / cargo / Homebrew put things.
source "$HOME/.cargo/env" 2>/dev/null
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

# Some networks break cargo's bundled-libcurl connection to crates.io, which
# shows up as "Error in the HTTP2 framing layer" or "SSL_ERROR_SYSCALL". Force
# HTTP/1.1, add retries, and fetch the crate index with the system git client
# (which uses your working system TLS instead of cargo's bundled libcurl).
export CARGO_HTTP_MULTIPLEXING=false
export CARGO_NET_RETRY=10
export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=git
export CARGO_NET_GIT_FETCH_WITH_CLI=true

echo "=============================="
echo " Building Notekeeper"
echo "=============================="
echo

# --- prerequisite checks -------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  echo "❌ Rust (cargo) is not installed."
  echo "   Install it once with:"
  echo "     curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  echo
  echo "Press any key to close."
  read -n 1 -s
  exit 1
fi

if ! cargo tauri --version >/dev/null 2>&1; then
  echo "ℹ️  The Tauri CLI isn't installed yet — installing it now (one-time)…"
  echo
  if ! cargo install tauri-cli --version "^2"; then
    echo
    echo "❌ Could not install the Tauri CLI. See the output above."
    echo "Press any key to close."
    read -n 1 -s
    exit 1
  fi
fi

# Generate the icon set the first time (cargo tauri build needs it).
if [ ! -d "src-tauri/icons" ]; then
  echo "ℹ️  Generating app icons from app-icon.png…"
  cargo tauri icon app-icon.png || true
  echo
fi

# --- build ---------------------------------------------------------------
APP="src-tauri/target/release/bundle/macos/Notekeeper.app"
MACOS_BUNDLE_DIR="src-tauri/target/release/bundle/macos"
DMG_DIR="src-tauri/target/release/bundle/dmg"

# The .dmg packaging step (bundle_dmg.sh) mounts a "Notekeeper" volume and runs an
# AppleScript via Finder to lay out the window. When a previous run failed partway
# it can leave behind a read-write scratch image (rw.<pid>.*.dmg) and/or a still-
# mounted "/Volumes/Notekeeper" — and the NEXT run then trips over that leftover
# state and fails again. Clearing it first turns a repeating failure back into a
# one-off, so the dmg step gets a clean shot each time.
cleanup_dmg_leftovers() {
  rm -f "$MACOS_BUNDLE_DIR"/rw.*.dmg 2>/dev/null
  if [ -d "/Volumes/Notekeeper" ]; then
    hdiutil detach "/Volumes/Notekeeper" -force >/dev/null 2>&1 || true
  fi
}

# `cargo tauri build` exits non-zero if EITHER the compile fails OR the cosmetic
# .dmg packaging fails — but the app bundle is produced before the dmg step, so a
# dmg-only failure still leaves a perfectly good Notekeeper.app. Keying success off
# the exit code therefore reports a false "Build failed" whenever only the dmg
# breaks. Instead, delete any stale app up front and treat "Notekeeper.app exists
# afterwards" as the real success signal (it can only exist if the compile passed).
run_build() {
  cleanup_dmg_leftovers
  rm -rf "$APP" 2>/dev/null
  CARGO_NET_OFFLINE=true cargo tauri build
  [ -d "$APP" ]
}

# If a previous build already downloaded the crates, they're cached locally, so
# try an OFFLINE build first: it uses that cache and never contacts crates.io,
# avoiding the network/SSL errors entirely. Only fall back to downloading if the
# offline build didn't produce the app (i.e. a missing crate, not a dmg hiccup).
build_succeeded=false

echo "▶  Attempt 1: offline build (uses crates already cached on this Mac)…"
echo
if run_build; then
  build_succeeded=true
else
  echo
  echo "ℹ️  The app bundle wasn't produced. The usual cause is that a few crates"
  echo "    (e.g. keyring) aren't cached yet — a SMALL download. Fetching them, then"
  echo "    building again…"
  echo
  # Download just the missing crate sources first. `cargo fetch` is much lighter
  # than a full build, so on a flaky connection it has many quick chances to slip
  # the small remaining download through. Anything fetched is cached permanently,
  # so each attempt resumes where the last left off.
  echo "▶  Fetching the missing crates (up to 8 quick tries)…"
  fetched=false
  fetch_try=1
  while [ "$fetched" = false ] && [ "$fetch_try" -le 8 ]; do
    echo "   fetch attempt $fetch_try/8…"
    if (cd src-tauri && cargo fetch); then
      fetched=true
    else
      fetch_try=$((fetch_try + 1))
      sleep 2
    fi
  done

  if [ "$fetched" = true ]; then
    echo
    echo "✅ All crates present. Building offline now…"
    echo
    if run_build; then
      build_succeeded=true
    fi
  else
    echo
    echo "⚠️  Still couldn't download the remaining crates after 8 tries."
  fi
fi

if [ "$build_succeeded" = false ]; then
  echo
  echo "❌ Build failed — the app bundle wasn't produced. Scroll up for the first error."
  echo "   If it's a network/SSL error to crates.io, it's almost always your"
  echo "   connection: a VPN, corporate proxy, or flaky Wi-Fi. Disable any VPN"
  echo "   or switch networks (a phone hotspot often works), then run this again."
  echo
  echo "Press any key to close."
  read -n 1 -s
  exit 1
fi

# --- done ----------------------------------------------------------------
# The app built. The .dmg is a cosmetic extra — report it only if it was actually
# produced, and don't fail the build when it wasn't.
cleanup_dmg_leftovers
echo
echo "✅ Build complete."
echo "   App:  $(cd "$(dirname "$APP")" && pwd)/Notekeeper.app"
# Match the final dmg by glob so this works on any arch (aarch64 / x86_64) and
# version. The rw.*.dmg scratch images live in the macos/ dir, not here, so this
# only ever picks up the finished disk image.
DMG="$(ls "$DMG_DIR"/*.dmg 2>/dev/null | head -1)"
if [ -n "$DMG" ] && [ -f "$DMG" ]; then
  echo "   DMG:  $(cd "$DMG_DIR" && pwd)/$(basename "$DMG")"
else
  echo
  echo "ℹ️  The .dmg wasn't created — only the app (which is all you need: just drag"
  echo "    it to /Applications). The dmg step fails when Terminal isn't allowed to"
  echo "    control Finder. To get the dmg too, grant it once in"
  echo "    System Settings → Privacy & Security → Automation → Terminal → Finder,"
  echo "    then run this again."
fi
echo
echo "Revealing the app in Finder. Drag Notekeeper.app into /Applications to install."
open -R "$APP"

echo
echo "Press any key to close."
read -n 1 -s
