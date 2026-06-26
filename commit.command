#!/bin/bash
# Double-click to stage everything, commit, and push to GitHub.
# Type a commit message, or just press Enter to use the default.

cd "$(dirname "$0")" || exit 1
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH"

echo "=============================="
echo " Commit & push — Notekeeper"
echo "=============================="
echo

# Must be inside a git repo.
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "❌ This folder isn't a git repository."
  echo "Press any key to close."
  read -n 1 -s
  exit 1
fi

# Clear a leftover lock from an interrupted git process, which would otherwise
# block every commit with "index.lock: File exists".
if [ -f .git/index.lock ]; then
  echo "⚠️  Found a leftover git lock (.git/index.lock) from an interrupted action."
  printf "   Remove it and continue? [y/N] "
  read -r ans
  if [ "$ans" = "y" ] || [ "$ans" = "Y" ]; then
    if rm -f .git/index.lock; then
      echo "   Removed."
    else
      echo "   ❌ Couldn't remove it — delete .git/index.lock manually, then retry."
      echo "Press any key to close."; read -n 1 -s; exit 1
    fi
  else
    echo "   Aborting without changes."
    echo "Press any key to close."; read -n 1 -s; exit 1
  fi
  echo
fi

# Nothing to commit? Stop early.
if [ -z "$(git status --porcelain)" ]; then
  echo "✅ Nothing to commit — your working tree is already clean."
  echo "Press any key to close."
  read -n 1 -s
  exit 0
fi

echo "These changes will be committed:"
echo "------------------------------"
git status --short
echo "------------------------------"
echo

# Ask for a message; Enter accepts the default.
default_msg="Update Notekeeper ($(date '+%Y-%m-%d %H:%M'))"
echo "Type a commit message and press Enter,"
echo "or just press Enter to use:  \"$default_msg\""
printf "> "
read -r msg
[ -z "$msg" ] && msg="$default_msg"
echo

# Stage, commit, push.
if ! git add -A; then
  echo "❌ 'git add' failed."
  echo "Press any key to close."; read -n 1 -s; exit 1
fi

if ! git commit -m "$msg"; then
  echo "❌ Commit failed — see the message above."
  echo "Press any key to close."; read -n 1 -s; exit 1
fi
echo

echo "▶  Pushing to GitHub…"
if ! git push; then
  echo
  echo "❌ Push failed. Common causes:"
  echo "   • First push of a new branch — run once in Terminal:"
  echo "       git push -u origin \$(git branch --show-current)"
  echo "   • Network down, or GitHub login/credentials needed."
  echo "Press any key to close."; read -n 1 -s; exit 1
fi

echo
echo "✅ Committed and pushed:"
echo "   \"$msg\""
echo
echo "Press any key to close."
read -n 1 -s
