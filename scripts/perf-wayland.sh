#!/usr/bin/env bash
# Timing gate on a real Wayland session, run by hand. scripts/ci.sh never runs it, because it opens windows on the desktop (each
# a short scripted run that quits by itself).
#
#   scripts/perf-wayland.sh [startup] [typing] [pdf]     default: all three
#
# Runs the release `lemma` with LEMMA_PERF (crates/lemma-app/src/perf.rs):
#   startup  three runs on target/fixtures/doc1k.tex: process start to the first frame, and to
#            the first frame with the document (medians);
#   typing   keystroke to frame on doc1k.tex and tests/fixtures/large.tex, outline sidebar shown;
#   pdf      PDF scroll frames on target/fixtures/pdf50.tex, three pages high and fit width;
# prints every `[perf]` line and checks the [perf] budgets of scripts/budgets.toml.
#
# Env: LEMMA_BIN        use this binary instead of building lemma-app
#      LEMMA_CONFIG     settings file (default: every optional feature on, see below)
#      LEMMA_PROFILE    cargo profile to build and run (default: release)
#      LEMMA_BUDGETS    budgets file (default: scripts/budgets.toml)
#      LEMMA_PERF_KEYS  keystrokes per typing run (default 200)
#      GSK_RENDERER     passed through, to compare renderers (default: the settings' choice)
#      LEMMA_PERF_FULLSCREEN=1  typing and PDF scrolling in a full-screen window (worst case)
#      LEMMA_PERF_BROADWAY=1  run offscreen under scripts/broadway.sh and only print (a smoke
#                       test of this script; Broadway paints about once a second)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() { echo "perf-wayland.sh: $*" >&2; exit 2; }

BUDGETS="${LEMMA_BUDGETS:-scripts/budgets.toml}"
PROFILE="${LEMMA_PROFILE:-release}"
[ -f "$BUDGETS" ] || die "no budgets file at $BUDGETS"
command -v python3 >/dev/null || die "python3 is required"

broadway=0
if [ "${LEMMA_PERF_BROADWAY:-0}" = 1 ]; then
  broadway=1
else
  [ -n "${WAYLAND_DISPLAY:-}" ] || die "needs a Wayland session (WAYLAND_DISPLAY is unset)"
fi

wanted=("$@")
[ ${#wanted[@]} -gt 0 ] || wanted=(startup typing pdf)
for w in "${wanted[@]}"; do
  case "$w" in startup | typing | pdf) ;; *) die "unknown scenario $w (startup, typing, pdf)" ;; esac
done

for f in target/fixtures/doc1k.tex target/fixtures/pdf50.pdf tests/fixtures/large.tex; do
  [ -e "$f" ] || die "missing $f (run scripts/build-fixtures.sh)"
done

if [ -n "${LEMMA_BIN:-}" ]; then
  [ -x "$LEMMA_BIN" ] || die "LEMMA_BIN=$LEMMA_BIN is not executable"
  bin="$LEMMA_BIN"
else
  target="${CARGO_TARGET_DIR:-$ROOT/target}"
  case "$target" in /*) ;; *) target="$ROOT/$target" ;; esac
  if [ "$PROFILE" = dev ] || [ "$PROFILE" = debug ]; then
    cargo build --quiet -p lemma-app >&2
    bin="$target/debug/lemma"
  else
    cargo build --quiet --profile "$PROFILE" -p lemma-app >&2
    bin="$target/$PROFILE/lemma"
  fi
fi

mkdir -p "$ROOT/target"
tmp="$(mktemp -d "$ROOT/target/perf.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
# No user state is read or written: settings from LEMMA_CONFIG, state and data in $tmp.
export XDG_STATE_HOME="$tmp/state" XDG_DATA_HOME="$tmp/data"
if [ -z "${LEMMA_CONFIG:-}" ]; then
  # The defaults have every feature on except these.
  cat >"$tmp/config.toml" <<'EOF'
[typing]
replacements = true
[pdf]
thumbnails = true
EOF
  export LEMMA_CONFIG="$tmp/config.toml"
fi

echo "perf-wayland.sh: $bin; renderer ${GSK_RENDERER:-from the settings}; config $LEMMA_CONFIG"
out="$tmp/perf.out"
: >"$out"

# run SCENARIOS ARGS...: one scripted run; its [perf] lines go to $out and the terminal.
run() {
  local scenarios="$1"
  shift
  local log="$tmp/run.log" status=0
  local runner=("$bin")
  [ "$broadway" = 1 ] && runner=(scripts/broadway.sh "$bin")
  LEMMA_PERF="$scenarios" LEMMA_PERF_T0="$(date +%s%N)" \
    "${runner[@]}" "$@" --quit-after-ms 1 </dev/null >"$log" 2>&1 || status=$?
  grep '^\[perf\]' "$log" | tee -a "$out" || true
  if [ "$status" != 0 ]; then
    echo "perf-wayland.sh: lemma $* failed (status $status):" >&2
    tail -n 30 "$log" >&2
    return 1
  fi
}

status=0
for w in "${wanted[@]}"; do
  case "$w" in
    startup)
      for _ in 1 2 3; do run startup target/fixtures/doc1k.tex || status=1; done
      ;;
    typing)
      run typing target/fixtures/doc1k.tex --sidebar outline || status=1
      run typing tests/fixtures/large.tex --sidebar outline || status=1
      ;;
    pdf)
      run pdf-scroll target/fixtures/pdf50.tex || status=1
      ;;
  esac
done

python3 - "$BUDGETS" "$out" "$broadway" <<'PY' || status=1
import statistics, sys, tomllib

budgets_path, out_path, broadway = sys.argv[1], sys.argv[2], sys.argv[3] == "1"
with open(budgets_path, "rb") as f:
    budgets = tomllib.load(f).get("perf", {})
tolerance = float(budgets.get("tolerance", 1.0))
runs = []
for line in open(out_path):
    words = line.split()
    if len(words) < 2:
        continue
    fields = dict(w.split("=", 1) for w in words[2:] if "=" in w)
    runs.append((words[1], fields))

failed = False

def check(what, value, budget, worse="higher"):
    global failed
    if budget is None:
        print(f"  {what}: {value:.2f} (no budget)")
        return
    # Frame rates are capped by the display, so they get no tolerance.
    limit = budget * tolerance if worse == "higher" else budget
    bad = value > limit if worse == "higher" else value < limit
    verdict = "ok" if not bad else ("over budget (not asserted under Broadway)" if broadway else "OVER BUDGET")
    if bad and not broadway:
        failed = True
    print(f"  {what}: {value:.2f} / budget {budget} (limit {limit:.2f})  {verdict}")

print("perf-wayland.sh: budgets (" + ("Broadway, printing only" if broadway else "asserted") + ")")
first = [float(f["first_frame_ms"]) for s, f in runs if s == "startup" and "first_frame_ms" in f]
if first:
    check("startup first frame ms (median)", statistics.median(first), budgets.get("first_frame_ms"))
ready = [float(f["ready_ms"]) for s, f in runs if s == "startup" and "ready_ms" in f]
if ready:
    check("startup document shown ms (median)", statistics.median(ready), budgets.get("ready_ms"))
for s, f in runs:
    if s == "typing":
        name = f.get("file", "?")
        check(f"typing {name} work p99 ms", float(f["work_p99"]), budgets.get("keystroke_work_p99_ms"))
        check(f"typing {name} keystroke to frame p99 ms", float(f["to_frame_p99"]), budgets.get("keystroke_to_frame_p99_ms"))
    elif s == "pdf-scroll":
        zoom = f.get("zoom", "?")
        check(f"pdf scroll {zoom} paint p99 ms", float(f["paint_p99"]), budgets.get("pdf_paint_p99_ms"))
        check(f"pdf scroll {zoom} fps", float(f["fps"]), budgets.get("pdf_fps"), worse="lower")
sys.exit(1 if failed else 0)
PY

exit "$status"
