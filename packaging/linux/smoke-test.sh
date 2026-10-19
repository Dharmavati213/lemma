#!/usr/bin/env bash
# Starts an installed Lemma on an offscreen GTK Broadway display and snapshots its window, as CI
# does after installing each Linux package in a fresh container.
#
#   packaging/linux/smoke-test.sh LEMMA OUT NAME     (OUT/NAME.png and OUT/NAME.log)
#
# Needs gtk4-broadwayd (Arch, Fedora: gtk4; Debian, Ubuntu: libgtk-4-bin).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
[ $# -eq 3 ] || { echo "usage: smoke-test.sh LEMMA OUT NAME" >&2; exit 2; }
lemma="$1"
mkdir -p "$2"
out="$(cd "$2" && pwd)"
name="$3"

"$lemma" --version
if ldd "$lemma" | grep 'not found'; then
  echo "smoke-test.sh: libraries are missing" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp -r "$ROOT/tests/fixtures/proj" "$work/proj"
mkdir -p "$work/state" "$work/run"
chmod 700 "$work/run"
status=0
XDG_RUNTIME_DIR="$work/run" LEMMA_CONFIG=/dev/null XDG_STATE_HOME="$work/state" \
  timeout 120 "$ROOT/scripts/broadway.sh" "$lemma" "$work/proj/main.tex" \
  --snapshot "$out/$name.png" --quit-after-ms 8000 >"$out/$name.log" 2>&1 || status=$?
if [ "$status" -ne 0 ] || [ ! -s "$out/$name.png" ]; then
  echo "smoke-test.sh: no snapshot (exit $status); output:" >&2
  tail -n 60 "$out/$name.log" >&2
  exit 1
fi
echo "ok: $out/$name.png"
