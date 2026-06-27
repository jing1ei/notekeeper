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
# If a previous build already downloaded the crates, they're cached locally, so
# try an OFFLINE build first: it uses that cache and never contacts crates.io,
# avoiding the network/SSL errors entirely. Only fall back to downloading if the
# offline build reports a missing crate.
build_succeeded=false

echo "▶  Attempt 1: offline build (uses crates already cached on this Mac)…"
echo
if CARGO_NET_OFFLINE=true cargo tauri build; then
  build_succeeded=true
else
  echo
  echo "ℹ️  Offline build didn't complete. Two common causes:"
  echo "     • a few crates (e.g. keyring) aren't cached yet (a SMALL download), or"
  echo "     • the .dmg packaging step (bundle_dmg.sh) hit a transient Finder/"
  echo "       hdiutil race — harmless, and it usually succeeds on the next run."
  echo "   Fetching any missing crates, then building again…"
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
    if CARGO_NET_OFFLINE=true cargo tauri build; then
      build_succeeded=true
    fi
  else
    echo
    echo "⚠️  Still couldn't download the remaining crates after 8 tries."
  fi
fi

if [ "$build_succeeded" = false ]; then
  echo
  echo "❌ Build failed — scroll up for the first error."
  echo "   If it's a network/SSL error to crates.io, it's almost always your"
  echo "   connection: a VPN, corporate proxy, or flaky Wi-Fi. Disable any VPN"
  echo "   or switch networks (a phone hotspot often works), then run this again."
  echo
  echo "Press any key to close."
  read -n 1 -s
  exit 1
fi

# --- done ----------------------------------------------------------------
APP="src-tauri/target/release/bundle/macos/Notekeeper.app"
echo
echo "✅ Build complete."
if [ -d "$APP" ]; then
  echo "   App:  $(cd "$(dirname "$APP")" && pwd)/Notekeeper.app"
  echo "   (a .dmg is alongside it under bundle/dmg/)"
  echo
  echo "Revealing it in Finder. Drag Notekeeper.app into /Applications to install."
  open -R "$APP"
else
  echo "   Bundle not found at the expected path — check the output above."
fi

echo
echo "Press any key to close."
read -n 1 -s
