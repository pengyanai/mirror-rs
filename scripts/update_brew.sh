#!/usr/bin/env bash
# Update the Homebrew formula in the tap repo to the latest mirror-rs release.
#
# Usage:
#   ./scripts/update_brew.sh [tag]
#
#   tag  optional release tag, e.g. v0.1.0 (default: latest release)
#
# The tap repo is cloned (or pulled) at $TAP_DIR (default: ~/homebrew-tap).
# Requires git auth to push to pengyanai/homebrew-tap (gh credential helper works).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAG="${1:-}"
TAP_DIR="${TAP_DIR:-$HOME/homebrew-tap}"

if [ ! -d "$TAP_DIR/.git" ]; then
  git clone https://github.com/pengyanai/homebrew-tap.git "$TAP_DIR"
fi
git -C "$TAP_DIR" pull --ff-only

python3 "$REPO_ROOT/scripts/gen_brew_formula.py" "$TAG" "$TAP_DIR/Formula/mirror-rs.rb"

git -C "$TAP_DIR" add Formula/mirror-rs.rb
if git -C "$TAP_DIR" diff --cached --quiet; then
  echo "Formula unchanged."
else
  git -C "$TAP_DIR" -c user.name="qian.cheng" -c user.email="mgr.qiancheng@gmail.com" \
    commit -m "mirror-rs: bump to latest release"
  git -C "$TAP_DIR" push
  echo "Pushed updated formula."
fi
