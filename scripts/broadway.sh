#!/usr/bin/env bash
# Run a command against an offscreen GTK Broadway display so GUI tests never open windows
# on your Wayland desktop.  Usage: scripts/broadway.sh <command> [args...]
# The daemon is shared and started on demand (display :7 unless LEMMA_BROADWAY_DISPLAY is set).
set -euo pipefail
DISP="${LEMMA_BROADWAY_DISPLAY:-:7}"
SOCK="${XDG_RUNTIME_DIR:-/tmp}/broadway$(( ${DISP#:} + 1 )).socket"   # broadwayd uses display+1
if [ ! -S "$SOCK" ]; then
  nohup gtk4-broadwayd "${DISP}" >/dev/null 2>&1 &
  for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
fi
export GDK_BACKEND=broadway BROADWAY_DISPLAY="${DISP}"
unset WAYLAND_DISPLAY DISPLAY
# GTK's own input method: with an IBus or Fcitx module installed, GTK would pick it and talk to
# the desktop's input method daemon, which answers keys asynchronously (racy real-key tests).
export GTK_IM_MODULE=gtk-im-context-simple
exec "$@"
