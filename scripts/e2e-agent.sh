#!/usr/bin/env bash
# End-to-end runs of the real `lemma` binary against tools/lemma-fake-agent (never the real
# agent), offscreen under scripts/broadway.sh. Each case copies tests/fixtures/proj into a fresh
# directory, runs `lemma main.tex --demo agent --agent-scenario <scenario> --snapshot …`, and
# checks the demo's report line (`agent-demo: key=value …`, see src/testmode/agent.rs) and the
# files on disk.
#
#   scripts/e2e-agent.sh [case...]      default: every case below
#
# Cases: basic, inline (Ctrl+K on the abstract), permission (card left pending),
#        permission-y, permission-Y, permission-n, bypass, errors, cancel, cancel-permission,
#        long.
#
# Env: LEMMA_BIN       use this binary (else lemma-app, lemma-fake-agent and lemma-mcp are built
#                      with LEMMA_PROFILE, default dev, into CARGO_TARGET_DIR)
#      LEMMA_E2E_DIR   where the copies and snapshots go (default: target/e2e-agent)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() { echo "e2e-agent.sh: $*" >&2; exit 2; }

PROFILE="${LEMMA_PROFILE:-dev}"
OUT="${LEMMA_E2E_DIR:-$ROOT/target/e2e-agent}"
case "$OUT" in /*) ;; *) OUT="$ROOT/$OUT" ;; esac

if [ -n "${LEMMA_BIN:-}" ]; then
  bin="$LEMMA_BIN"
else
  target="${CARGO_TARGET_DIR:-$ROOT/target}"
  case "$target" in /*) ;; *) target="$ROOT/$target" ;; esac
  if [ "$PROFILE" = dev ] || [ "$PROFILE" = debug ]; then
    cargo build --quiet -p lemma-app -p lemma-fake-agent -p lemma-mcp
    bin="$target/debug/lemma"
  else
    cargo build --quiet --profile "$PROFILE" -p lemma-app -p lemma-fake-agent -p lemma-mcp
    bin="$target/$PROFILE/lemma"
  fi
fi
[ -x "$bin" ] || die "lemma binary not found at $bin"
mkdir -p "$OUT"

failures=0
report=""

# run CASE SCENARIO [VAR=value...]: copies the fixture and runs the demo; sets $report and $dir.
run() {
  local name="$1" scenario="$2"
  shift 2
  dir="$OUT/$name"
  rm -rf "${dir:?}"
  mkdir -p "$dir"
  cp -r tests/fixtures/proj/. "$dir/"
  local log="$OUT/$name.log"
  mkdir -p "$OUT/state"
  if ! env LEMMA_CONFIG=/dev/null XDG_STATE_HOME="$OUT/state" "$@" scripts/broadway.sh "$bin" "$dir/main.tex" \
      --demo agent --agent-scenario "$ROOT/tests/fixtures/agent/$scenario" \
      --snapshot "$OUT/$name.png" --quit-after-ms 1000 </dev/null >"$log" 2>&1; then
    echo "FAIL $name: lemma exited with an error; see $log"
    failures=$((failures + 1))
    report=""
    return 1
  fi
  report="$(grep '^agent-demo:' "$log" | tail -n 1)"
  [ -n "$report" ] || { echo "FAIL $name: no report line; see $log"; failures=$((failures + 1)); return 1; }
}

# expect CASE FIELD=VALUE...: every field must appear in the report with exactly that value.
expect() {
  local name="$1" ok=1 want
  shift
  for want in "$@"; do
    if [[ " $report " != *" $want "* ]]; then
      echo "FAIL $name: expected $want"
      ok=0
    fi
  done
  if [ "$ok" = 1 ]; then
    echo "ok   $name: ${report#agent-demo: }"
  else
    echo "     report: $report"
    failures=$((failures + 1))
  fi
}

# contains CASE FILE TEXT: the copied FILE contains TEXT (the agent's edit reached the disk).
contains() {
  if ! grep -qF -- "$3" "$dir/$2"; then
    echo "FAIL $1: $2 does not contain: $3"
    failures=$((failures + 1))
  fi
}

case_basic() {
  run basic basic.json || return 0
  expect basic stop=EndTurn tools=4 tool_errors=0 builds=1 build=Success \
    hunks=chapters/yoneda.tex:1,main.tex:2 disk=same
  contains basic main.tex "These notes serve as a test document for Lemma."
  contains basic main.tex "is treated in detail in~\\cite{Vistoli}."
  contains basic chapters/yoneda.tex "Throughout, let \$\\cC\$ be a locally small category"
}
case_inline() {
  run inline basic.json LEMMA_DEMO_INLINE="Say that the notes are short" LEMMA_DEMO_LINE=34 || return 0
  expect inline stop=EndTurn tools=1 tool_errors=0 hunks=main.tex:1 disk=same
  contains inline main.tex "These notes are a short test document."
}
case_permission() {
  run permission permission.json || return 0
  expect permission stop=permission permissions=1 cards=pending hunks=-
}
case_permission-y() {
  run permission-y permission.json LEMMA_DEMO_ANSWER=y LEMMA_DEMO_PROMPT="run it once" || return 0
  expect permission-y stop=EndTurn permissions=1 cards=allow-once tool_errors=0
}
case_permission-Y() {
  run permission-Y permission.json LEMMA_DEMO_ANSWER=Y LEMMA_DEMO_PROMPT="allow it always" || return 0
  expect permission-Y stop=EndTurn permissions=1 cards=allow-with-updates tool_errors=0
}
case_permission-n() {
  run permission-n permission.json LEMMA_DEMO_ANSWER=n LEMMA_DEMO_PROMPT="reject that" || return 0
  expect permission-n stop=EndTurn permissions=1 cards=reject
}
case_bypass() {
  run bypass bypass.json || return 0
  expect bypass stop=EndTurn tools=2 builds=1 build=Success \
    hunks=chapters/yoneda.tex:1,main.tex:1 disk=same
  contains bypass chapters/yoneda.tex "is fully faithful, hence an embedding."
}
case_errors() {
  run errors errors.json || return 0
  expect errors stop=EndTurn tools=3 tool_errors=3 builds=0 hunks=- disk=same
}
case_cancel() {
  run cancel cancel.json LEMMA_DEMO_CANCEL_MS=500 || return 0
  expect cancel stop=Cancelled builds=0 hunks=-
  local ms
  ms="$(sed -n 's/.* cancelled_ms=\([0-9]*\) .*/\1/p' <<<"$report")"
  if [ -z "$ms" ] || [ "$ms" -gt 500 ]; then
    echo "FAIL cancel: the turn ended ${ms:-?} ms after Stop (limit 500)"
    failures=$((failures + 1))
  fi
}
case_cancel-permission() {
  run cancel-permission cancel.json LEMMA_DEMO_CANCEL_MS=800 LEMMA_DEMO_PROMPT="permission please" || return 0
  expect cancel-permission stop=Cancelled permissions=1 cards=cancelled
}
case_long() {
  run long long.json || return 0
  expect long stop=EndTurn chunks=200 tools=31 tool_errors=0 builds=1 \
    hunks=long-scenario.tex:1 disk=same
  # Review opened the new file in a tab, at its first line.
  if [[ " $report " != *" view=long-scenario.tex:1-"* ]]; then
    echo "FAIL long: the new file's first lines are not on screen"
    failures=$((failures + 1))
  fi
}

all=(basic inline permission permission-y permission-Y permission-n bypass errors cancel cancel-permission long)
cases=("$@")
[ ${#cases[@]} -gt 0 ] || cases=("${all[@]}")
for c in "${cases[@]}"; do
  declare -F "case_$c" >/dev/null || die "unknown case $c (known: ${all[*]})"
  "case_$c"
done

echo
if [ "$failures" -gt 0 ]; then
  echo "e2e-agent.sh: $failures failure(s); logs and snapshots in $OUT"
  exit 1
fi
echo "e2e-agent.sh: all ${#cases[@]} case(s) passed; snapshots in $OUT"
