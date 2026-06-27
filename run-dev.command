#!/bin/bash
# Double-click to run Notekeeper from the CURRENT source (no install, no stale
# /Applications copy). It first makes sure all crates are downloaded, then runs
# the app with the developer console open so errors are visible.

cd "$(dirname "$0")" || exit 1
source "$HOME/.cargo/env" 2>/dev/null
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

# Resilience for flaky networks when fetching the few missing crates.
export CARGO_HTTP_MULTIPLEXING=false
export CARGO_NET_RETRY=10
export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=git
export CARGO_NET_GIT_FETCH_WITH_CLI=true

echo "=============================="
echo " Notekeeper — dev run"
echo "=============================="
echo

# Make sure dependencies are present. This needs the internet ONCE to grab the
# few crates that aren't cached (keyring + a couple of small deps). After that
# it's instant. Retry a few times to ride out a flaky connection.
echo "▶  Making sure all crates are downloaded (needs internet the first time)…"
fetched=false
for try in 1 2 3 4 5 6; do
  echo "   fetch attempt $try/6…"
  if (cd src-tauri && cargo fetch); then
    fetched=true
    break
  fi
  sleep 2
done

if [ "$fetched" = false ]; then
  echo
  echo "❌ Couldn't download the missing crates."
  echo "   This is the ONLY thing blocking you — it's ~4 small crates."
  echo "   Connect to a different network (a phone hotspot works well) or turn"
  echo "   off any VPN, then run this again. After it downloads once, you're set."
  echo
  echo "Press any key to close."
  read -n 1 -s
  exit 1
fi

echo
echo "✅ All crates present."
echo

# Generate the icon set if it's missing (a fresh clone may not have it). Tauri
# reads these at dev-run time for the window/tray icon, so without them the app
# fails to start.
if [ ! -d "src-tauri/icons" ]; then
  echo "ℹ️  Generating app icons from app-icon.png…"
  cargo tauri icon app-icon.png || true
  echo
fi

echo "Launching the app…"
echo
echo "WHEN THE APP STARTS:"
echo "  1. No Dock icon — click the Notekeeper icon in the menu bar (top-right),"
echo "     then choose 'Open Manager'."
echo "  2. A developer console pops up automatically (dev builds only)."
echo "  3. Click '+ Add bot'. If nothing happens, open the console's 'Console'"
echo "     tab and send me any RED error text."
echo
echo "To stop: quit the app (menu bar icon -> Quit) or press Ctrl-C here."
echo "------------------------------"
echo

cargo tauri dev

echo
echo "Dev session ended. Press any key to close."
read -n 1 -s
