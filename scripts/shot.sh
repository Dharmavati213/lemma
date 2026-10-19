#!/usr/bin/env bash
# Screenshot of Lemma.
#
#   scripts/shot.sh OUT.png [lemma args...]
#   e.g. scripts/shot.sh /tmp/m0.png tests/fixtures/proj/main.tex
# Paths are relative to the current directory.
#
# Default (safe for agents): runs `lemma <args> --snapshot OUT.png --quit-after-ms <ms>` under
# scripts/broadway.sh. The app paints its own window into the PNG, offscreen; nothing appears on
# the desktop.
#
# LEMMA_SHOT_WAYLAND=1 (by hand only): launches lemma on the real Wayland session, waits, grabs
# its Hyprland window (the active window if lemma's cannot be found) with grim, then quits lemma.
#
# Env: LEMMA_BIN      use this binary instead of building lemma-app
#      LEMMA_PROFILE  cargo profile to build and run (default: dev)
#      LEMMA_SHOT_MS  milliseconds before the snapshot or grab (default: 1500)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() { echo "shot.sh: $*" >&2; exit 2; }

[ $# -ge 1 ] || die "usage: scripts/shot.sh OUT.png [lemma args...]"
out="$1"
shift
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
mkdir -p "$(dirname "$out")"

PROFILE="${LEMMA_PROFILE:-dev}"
DELAY_MS="${LEMMA_SHOT_MS:-1500}"
[[ "$DELAY_MS" =~ ^[0-9]+$ ]] || die "LEMMA_SHOT_MS must be a number of milliseconds"

# Builds lemma-app (unless LEMMA_BIN is set) and prints the binary's path.
lemma_bin() {
  if [ -n "${LEMMA_BIN:-}" ]; then
    [ -x "$LEMMA_BIN" ] || die "LEMMA_BIN=$LEMMA_BIN is not executable"
    printf '%s\n' "$LEMMA_BIN"
    return
  fi
  local target="${CARGO_TARGET_DIR:-$ROOT/target}"
  # cargo resolves a relative CARGO_TARGET_DIR against the current directory.
  case "$target" in /*) ;; *) target="$PWD/$target" ;; esac
  if [ "$PROFILE" = dev ] || [ "$PROFILE" = debug ]; then
    cargo build --quiet --manifest-path "$ROOT/Cargo.toml" -p lemma-app >&2
    printf '%s\n' "$target/debug/lemma"
  else
    cargo build --quiet --manifest-path "$ROOT/Cargo.toml" --profile "$PROFILE" -p lemma-app >&2
    printf '%s\n' "$target/$PROFILE/lemma"
  fi
}

bin="$(lemma_bin)"
[ -x "$bin" ] || die "lemma binary not found at $bin"
rm -f "$out"

if [ "${LEMMA_SHOT_WAYLAND:-0}" = 1 ]; then
  [ -n "${WAYLAND_DISPLAY:-}" ] || die "LEMMA_SHOT_WAYLAND=1 needs a Wayland session"
  for tool in grim hyprctl jq; do
    command -v "$tool" >/dev/null || die "LEMMA_SHOT_WAYLAND=1 needs $tool"
  done
  "$bin" "$@" </dev/null &
  pid=$!
  trap 'kill "$pid" 2>/dev/null || true' EXIT
  sleep "$(awk -v ms="$DELAY_MS" 'BEGIN { printf "%.3f", ms / 1000 }')"
  kill -0 "$pid" 2>/dev/null || die "lemma exited before the screenshot"
  geometry='"\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"'
  region="$(hyprctl -j clients | jq -r --argjson pid "$pid" ".[] | select(.pid == \$pid) | $geometry" | head -n 1)"
  [ -n "$region" ] || region="$(hyprctl -j activewindow | jq -r "$geometry")"
  grim -g "$region" "$out"
else
  "$ROOT/scripts/broadway.sh" "$bin" "$@" --snapshot "$out" --quit-after-ms "$DELAY_MS" </dev/null
fi

[ -s "$out" ] || die "no screenshot was written to $out"
echo "$out ($(wc -c <"$out") bytes)"
