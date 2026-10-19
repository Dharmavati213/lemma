#!/usr/bin/env bash
# Starts the bundled Lemma.app for real with Homebrew moved out of the way (so every library and
# data file must come from the bundle) and snapshots its window: the PNGs and logs land in OUT.
#
#   packaging/macos/smoke-test.sh dist/Lemma.app [OUT]     (OUT default: dist/smoke)
#
# For CI (GitHub's macOS runners have a window server and password-less sudo): it renames
# Homebrew's prefix while the app runs, and renames it back on exit. Written for bash 3.2.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
die() { echo "smoke-test.sh: $*" >&2; exit 1; }

[ -d "${1:-}" ] || die "usage: smoke-test.sh Lemma.app [OUT]"
APP="$(cd "$1" && pwd)"
OUT="${2:-$ROOT/dist/smoke}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
LEMMA="$APP/Contents/MacOS/lemma"
BREW="$(brew --prefix)"

work="$(mktemp -d)"
cp -R "$ROOT/tests/fixtures/proj" "$work/proj"
mkdir -p "$work/state"

hidden=""
restore() {
  if [ -n "$hidden" ]; then
    sudo mv "$hidden" "$BREW"
  fi
  rm -rf "$work"
}
trap restore EXIT
hidden="$BREW.hidden-for-smoke-test"
sudo mv "$BREW" "$hidden"
echo "Homebrew moved aside: $BREW -> $hidden"

# run_for SECONDS CMD...: CMD, killed after SECONDS (macOS has no `timeout`).
run_for() {
  local secs="$1" waited=0
  shift
  "$@" &
  local pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge "$secs" ]; then
      kill -9 "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      return 124
    fi
    sleep 1
    waited=$((waited + 1))
  done
  wait "$pid"
}

failures=0
# shot NAME QUIT_MS LEMMA_ARGS...: one run; the snapshot is OUT/NAME.png, the output OUT/NAME.log.
shot() {
  local name="$1" ms="$2"
  shift 2
  rm -f "$OUT/$name.png"
  local status=0
  run_for 120 env LEMMA_CONFIG=/dev/null XDG_STATE_HOME="$work/state" LEMMA_LOG=lemma_app=info \
    "$LEMMA" "$@" --snapshot "$OUT/$name.png" --quit-after-ms "$ms" \
    </dev/null >"$OUT/$name.log" 2>&1 || status=$?
  if [ "$status" -eq 0 ] && [ -s "$OUT/$name.png" ]; then
    echo "ok   $name.png"
  else
    echo "FAIL $name.png (exit $status); output:"
    tail -n 80 "$OUT/$name.log" | sed 's/^/     /'
    failures=$((failures + 1))
  fi
}

"$LEMMA" --version
shot window 8000 "$work/proj/main.tex"
shot outline 8000 "$work/proj/main.tex" --demo outline
shot hover-ref 8000 "$work/proj/main.tex" --demo hover-ref
shot completion 8000 --demo completion
shot prefs 8000 --demo prefs

# Nothing may have come from Homebrew's prefix.
if grep -h -F -e "$hidden" -e "$BREW/" "$OUT"/*.log; then
  echo "FAIL: the app looked for files under Homebrew's prefix (above)"
  failures=$((failures + 1))
fi
[ "$failures" -eq 0 ] || die "$failures failure(s)"
echo "smoke-test.sh: all runs passed"
