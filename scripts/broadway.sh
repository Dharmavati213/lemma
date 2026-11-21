#!/usr/bin/env bash
# Run a command against an offscreen GTK Broadway display so GUI tests never open windows
# on your Wayland desktop.  Usage: scripts/broadway.sh <command> [args...]
# The daemon is shared and started on demand (display :7 unless LEMMA_BROADWAY_DISPLAY is set).
set -euo pipefail
DISP="${LEMMA_BROADWAY_DISPLAY:-:7}"
# broadwayd's socket is broadway<display + 1>.socket in GLib's runtime directory. Where
# XDG_RUNTIME_DIR is not set (a container, a `su` session) GLib takes the cache directory, which
# follows HOME and XDG_CACHE_HOME: a test that changes those would look for the display
# elsewhere. So the daemon and the command get one runtime directory then (scripts/ci.sh has
# the same lines).
if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
  XDG_RUNTIME_DIR="${TMPDIR:-/tmp}/lemma-runtime-$(id -u)"
  mkdir -p "$XDG_RUNTIME_DIR"
  chmod 700 "$XDG_RUNTIME_DIR"
  export XDG_RUNTIME_DIR
fi
SOCK="$XDG_RUNTIME_DIR/broadway$(( ${DISP#:} + 1 )).socket"
if [ ! -S "$SOCK" ]; then
  # Without it every GTK program would fail later with a bare "cannot open display".
  command -v gtk4-broadwayd >/dev/null || {
    echo "broadway.sh: gtk4-broadwayd is not installed (Arch, Fedora: gtk4; Debian, Ubuntu:" \
      "libgtk-4-bin)" >&2
    exit 1
  }
  nohup gtk4-broadwayd "${DISP}" >/dev/null 2>&1 &
  for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
fi
export GDK_BACKEND=broadway BROADWAY_DISPLAY="${DISP}"
unset WAYLAND_DISPLAY DISPLAY
# GTK's own input method: with an IBus or Fcitx module installed, GTK would pick it and talk to
# the desktop's input method daemon, which answers keys asynchronously (racy real-key tests).
export GTK_IM_MODULE=gtk-im-context-simple
# The en_US dictionary of scripts/fetch-dictionary.sh, on a machine with none of its own: enchant
# reads the hunspell dictionaries of its configuration directory. (The tests that wait for spell
# checking print SKIP without an English dictionary.)
DICTS="${CARGO_TARGET_DIR:-$(cd "$(dirname "$0")/.." && pwd)/target}/dictionaries"
if [ -z "${ENCHANT_CONFIG_DIR:-}" ] && [ -f "$DICTS/hunspell/en_US.dic" ]; then
  installed=0
  for dir in /usr/share/hunspell /usr/share/myspell /usr/share/myspell/dicts \
    "${XDG_CONFIG_HOME:-$HOME/.config}/enchant/hunspell"; do
    [ -f "$dir/en_US.dic" ] && installed=1
  done
  [ "$installed" = 1 ] || export ENCHANT_CONFIG_DIR="$DICTS"
fi
exec "$@"
