#!/usr/bin/env bash
# Memory check against scripts/budgets.toml (docs/ARCHITECTURE.md §7).
#
#   scripts/rss.sh [scenario...]        default: every scenario in budgets.toml
#
# Each scenario runs `lemma <args> --mem-report --quit-after-ms <ms>` and reads the last `Rss:` and
# `Pss:` lines (smaps_rollup format, in kB) that --mem-report prints on stdout or stderr.
#
# By default lemma runs offscreen under scripts/broadway.sh and the numbers are only printed:
# Broadway's memory profile differs from Wayland's, so budgets are not asserted there.
# LEMMA_RSS_WAYLAND=1 runs on the real Wayland session and fails when a value exceeds
# budget × tolerance. That opens windows on the desktop, so run it by hand.
#
# Env: LEMMA_BIN      use this binary instead of building lemma-app
#      LEMMA_CONFIG   settings file (default: /dev/null, the built-in defaults)
#      LEMMA_PROFILE  cargo profile to build and run (default: release)
#      LEMMA_BUDGETS  budgets file (default: scripts/budgets.toml)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() { echo "rss.sh: $*" >&2; exit 2; }

BUDGETS="${LEMMA_BUDGETS:-scripts/budgets.toml}"
PROFILE="${LEMMA_PROFILE:-release}"
[ -f "$BUDGETS" ] || die "no budgets file at $BUDGETS"
command -v python3 >/dev/null || die "python3 is required to read $BUDGETS"

assert=0
if [ "${LEMMA_RSS_WAYLAND:-0}" = 1 ]; then
  [ -n "${WAYLAND_DISPLAY:-}" ] || die "LEMMA_RSS_WAYLAND=1 needs a Wayland session (WAYLAND_DISPLAY is unset)"
  assert=1
fi

# Builds lemma-app (unless LEMMA_BIN is set) and prints the binary's path.
lemma_bin() {
  if [ -n "${LEMMA_BIN:-}" ]; then
    [ -x "$LEMMA_BIN" ] || die "LEMMA_BIN=$LEMMA_BIN is not executable"
    printf '%s\n' "$LEMMA_BIN"
    return
  fi
  local target="${CARGO_TARGET_DIR:-$ROOT/target}"
  case "$target" in /*) ;; *) target="$ROOT/$target" ;; esac
  if [ "$PROFILE" = dev ] || [ "$PROFILE" = debug ]; then
    cargo build --quiet -p lemma-app >&2
    printf '%s\n' "$target/debug/lemma"
  else
    cargo build --quiet --profile "$PROFILE" -p lemma-app >&2
    printf '%s\n' "$target/$PROFILE/lemma"
  fi
}

# One line per scenario, fields separated by \x1e and args by \x1f:
# name, rss_mb, pss_mb ("-" when there is no budget), quit_after_ms, tolerance, args.
scenarios="$(python3 - "$BUDGETS" "$@" <<'PY'
import sys, tomllib

path, wanted = sys.argv[1], sys.argv[2:]
with open(path, "rb") as f:
    cfg = tomllib.load(f)
tolerance = float(cfg.get("tolerance", 1.15))
# [perf] holds the timing budgets of scripts/perf-wayland.sh, not a memory scenario.
known = [name for name, value in cfg.items() if isinstance(value, dict) and name != "perf"]
unknown = [w for w in wanted if w not in known]
if unknown:
    sys.exit(f"rss.sh: unknown scenario(s) {', '.join(unknown)}; known: {', '.join(known)}")
for name in wanted or known:
    s = cfg[name]
    fields = [
        name,
        str(s.get("rss_mb", "-")),
        str(s.get("pss_mb", "-")),
        str(int(s.get("quit_after_ms", 3000))),
        str(tolerance),
        "\x1f".join(s.get("args", [])),
    ]
    print("\x1e".join(fields))
PY
)" || exit 2

bin="$(lemma_bin)"
[ -x "$bin" ] || die "lemma binary not found at $bin"
if [ "$assert" = 1 ]; then
  runner=("$bin")
  echo "rss.sh: Wayland session, asserting budgets ($bin)"
else
  runner=(scripts/broadway.sh "$bin")
  echo "rss.sh: Broadway, printing only; set LEMMA_RSS_WAYLAND=1 on Wayland to assert ($bin)"
fi

# The runs read no user settings (LEMMA_CONFIG=/dev/null unless set: the defaults, spell
# checking on) and write no state outside a temporary XDG_STATE_HOME under target/.
export LEMMA_CONFIG="${LEMMA_CONFIG:-/dev/null}"
mkdir -p "$ROOT/target"
tmp="$(mktemp -d "$ROOT/target/rss.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
log="$tmp/run.log"
export XDG_STATE_HOME="$tmp/state"
status=0

# Prints "<value> MB / budget <b> MB (limit <l>)" and returns 1 when over the limit.
check() {
  local kb="$1" budget="$2" tolerance="$3"
  awk -v kb="$kb" -v b="$budget" -v t="$tolerance" 'BEGIN {
    mb = kb / 1024
    if (b == "-") { printf "%7.1f MB (no budget)", mb; exit 0 }
    printf "%7.1f MB / budget %s MB (limit %.1f)", mb, b, b * t
    exit ((mb > b * t) ? 1 : 0)
  }'
}

while IFS=$'\x1e' read -r name rss_mb pss_mb quit_ms tolerance args_joined; do
  args=()
  [ -n "$args_joined" ] && IFS=$'\x1f' read -r -a args <<<"$args_joined"
  for arg in "${args[@]}"; do
    if [[ "$arg" != -* && "$arg" == */* && ! -e "$arg" ]]; then
      die "$name: missing $arg (run scripts/build-fixtures.sh)"
    fi
  done

  if ! "${runner[@]}" "${args[@]}" --mem-report --quit-after-ms "$quit_ms" </dev/null >"$log" 2>&1; then
    echo "$name: lemma failed; output:" >&2
    cat "$log" >&2
    status=1
    continue
  fi
  rss_kb="$(awk '$1 == "Rss:" { v = $2 } END { print v }' "$log")"
  pss_kb="$(awk '$1 == "Pss:" { v = $2 } END { print v }' "$log")"
  if [ -z "$rss_kb" ] || [ -z "$pss_kb" ]; then
    echo "$name: no 'Rss:'/'Pss:' lines in the --mem-report output:" >&2
    cat "$log" >&2
    status=1
    continue
  fi

  verdict="ok"
  rss_line="$(check "$rss_kb" "$rss_mb" "$tolerance")" || verdict="OVER BUDGET"
  pss_line="$(check "$pss_kb" "$pss_mb" "$tolerance")" || verdict="OVER BUDGET"
  if [ "$assert" = 0 ]; then
    [ "$verdict" = ok ] || verdict="over budget (not asserted under Broadway)"
  elif [ "$verdict" != ok ]; then
    status=1
  fi
  printf '%-8s RSS %s  PSS %s  %s\n' "$name" "$rss_line" "$pss_line" "$verdict"
done <<<"$scenarios"

exit "$status"
