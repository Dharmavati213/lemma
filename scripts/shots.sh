#!/usr/bin/env bash
# Regenerates the documentation screenshots (`all` below) into target/shots, offscreen
# under scripts/broadway.sh. Runs that write files work on copies of the fixtures in a temporary
# directory; the agent shots use tools/lemma-fake-agent (never the real agent).
#
#   scripts/shots.sh [name...]       default: every shot (names without .png)
#
# Env: LEMMA_BIN        use this binary (else lemma-app, lemma-fake-agent and lemma-mcp are built
#                       with LEMMA_PROFILE, default release, into CARGO_TARGET_DIR)
#      LEMMA_SHOTS_DIR  output directory (default: target/shots)
#      LEMMA_SHOT_MS    --quit-after-ms of each run (default: 1500)
#
# b-review-turns.png comes from tests/gtk_review_persist.rs (two agent turns need two prompts),
# run with LEMMA_TEST_SNAPSHOTS; so do the t-* shots (into target/shots/tests/: the history
# timeline, chktex findings, the label tools' popovers and dialogs, unmatched delimiters; M5: the
# environment tools and align helper, the agent backend picker and API key row). d-demo-pdf.png and the l-pdf-* shots need
# scripts/build-fixtures.sh (run on demand). p-git-gutter.png commits a copy of proj into a
# throwaway git repository (no global or system git configuration is read).
#
# Every run gets LEMMA_CONFIG=/dev/null (m-prefs: an empty temporary config.toml) and a temporary
# XDG_STATE_HOME; the fixture copies and the logs live in a temporary directory under target/.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() { echo "shots.sh: $*" >&2; exit 2; }

PROFILE="${LEMMA_PROFILE:-release}"
OUT="${LEMMA_SHOTS_DIR:-$ROOT/target/shots}"
case "$OUT" in /*) ;; *) OUT="$ROOT/$OUT" ;; esac
MS="${LEMMA_SHOT_MS:-1500}"
[[ "$MS" =~ ^[0-9]+$ ]] || die "LEMMA_SHOT_MS must be a number of milliseconds"

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
mkdir -p "$OUT" "$ROOT/target"
work="$(mktemp -d "$ROOT/target/shots-work.XXXXXX")"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/state"
agent="$ROOT/tests/fixtures/agent"
failures=0

# copy FIXTURE NAME: copies tests/fixtures/FIXTURE to $work/NAME and prints the copy's path.
copy() {
  rm -rf "${work:?}/${2:?}"
  cp -r "tests/fixtures/$1" "$work/$2"
  printf '%s\n' "$work/$2"
}

# shot NAME [VAR=value...] -- LEMMA_ARGS...: one run of lemma; the PNG is $OUT/NAME.png.
# QUIT_MS=n among the variables overrides the run's --quit-after-ms.
shot() {
  local name="$1" env=() ms="$MS"
  shift
  while [ "$1" != -- ]; do
    case "$1" in QUIT_MS=*) ms="${1#QUIT_MS=}" ;; *) env+=("$1") ;; esac
    shift
  done
  shift
  rm -f "$OUT/$name.png"
  if env LEMMA_CONFIG=/dev/null XDG_STATE_HOME="$work/state" "${env[@]}" \
      scripts/broadway.sh "$bin" "$@" --snapshot "$OUT/$name.png" --quit-after-ms "$ms" \
      </dev/null >"$work/$name.log" 2>&1 && [ -s "$OUT/$name.png" ]; then
    echo "ok   $name.png"
  else
    echo "FAIL $name.png; output (last 60 lines):"
    tail -n 60 "$work/$name.log" | sed 's/^/     /'
    failures=$((failures + 1))
  fi
}

light=(ADW_DEBUG_COLOR_SCHEME=prefer-light)

shot_a-build() { local d; d="$(copy proj a)"; shot a-build -- "$d/main.tex" --build; }
shot_a-build-light() { local d; d="$(copy proj al)"; shot a-build-light "${light[@]}" -- "$d/main.tex" --build; }
shot_b-review-three() {
  local d; d="$(copy review b3)"
  shot b-review-three -- "$d/main.tex" --apply-as-agent "$ROOT/tests/fixtures/review/reading_mode.json"
}
# All six review cases in one script.
all_cases() {
  python3 - "$ROOT/tests/fixtures/review" "$work/all.json" <<'PY'
import json, sys
src, out = sys.argv[1], sys.argv[2]
calls = []
for name in ["line1", "wrapped", "insertion", "deletion_mid", "word_change", "eof_deletion"]:
    with open(f"{src}/{name}.json") as f:
        calls += json.load(f)
with open(out, "w") as f:
    json.dump(calls, f)
PY
}
shot_b-review-all() { local d; d="$(copy review ba)"; all_cases; shot b-review-all -- "$d/main.tex" --apply-as-agent "$work/all.json"; }
shot_b-review-all-changes() {
  local d; d="$(copy review bac)"; all_cases
  shot b-review-all-changes -- "$d/main.tex" --apply-as-agent "$work/all.json" --sidebar changes
}
shot_c-errors-editor() { local d; d="$(copy logs/undefcs ce)"; shot c-errors-editor -- "$d/main.tex" --build; }
shot_c-errors-problems() { local d; d="$(copy logs/undefcs cp)"; shot c-errors-problems -- "$d/main.tex" --build --sidebar problems; }
shot_d-demo-pdf() {
  [ -f target/fixtures/pdf50.tex ] || scripts/build-fixtures.sh >/dev/null
  shot d-demo-pdf QUIT_MS=5000 -- --demo pdf "$ROOT/target/fixtures/pdf50.tex"
}
shot_e-agent-then-build() {
  local d; d="$(copy proj e)"
  cat >"$work/e.json" <<'JSON'
[{"tool": "edit", "file_path": "main.tex",
  "old_string": "These notes serve as a test document.",
  "new_string": "These notes serve as a test document for Lemma."}]
JSON
  shot e-agent-then-build -- "$d/main.tex" --apply-as-agent "$work/e.json" --build
}
shot_f-agent-panel() {
  local d; d="$(copy proj f)"
  shot f-agent-panel -- "$d/main.tex" --demo agent --agent-scenario "$agent/basic.json"
}
# A turn as the real CLI starts it: ToolSearch loads Lemma's deferred MCP tools (a quiet row),
# then built-in tools reported by their bare names (Read, Grep, Bash) and two of Lemma's tools.
shot_f-agent-tools() {
  local d; d="$(copy proj ft)"
  cat >"$work/ft.json" <<'JSON'
{"default": [
  {"raw_update": {"sessionUpdate": "tool_call", "toolCallId": "toolu_search", "title": "ToolSearch",
                  "kind": "other", "status": "pending",
                  "rawInput": {"query": "select:mcp__lemma__outline,mcp__lemma__get_diagnostics"}}},
  {"sleep_ms": 50},
  {"raw_update": {"sessionUpdate": "tool_call_update", "toolCallId": "toolu_search", "status": "completed"}},
  {"raw_update": {"sessionUpdate": "tool_call", "toolCallId": "toolu_read", "title": "Read",
                  "kind": "read", "status": "completed", "rawInput": {"file_path": "refs.bib"}}},
  {"raw_update": {"sessionUpdate": "tool_call", "toolCallId": "toolu_grep", "title": "Grep",
                  "kind": "search", "status": "completed", "rawInput": {"pattern": "label"}}},
  {"tool": {"name": "outline", "args": {}}},
  {"tool": {"name": "get_diagnostics", "args": {}}},
  {"raw_update": {"sessionUpdate": "tool_call", "toolCallId": "toolu_bash", "title": "Bash",
                  "kind": "execute", "status": "completed", "rawInput": {"command": "git status"}}},
  {"say": "Every `\\label` is referenced, and nothing needs to change."}
]}
JSON
  shot f-agent-tools -- "$d/main.tex" --demo agent --agent-scenario "$work/ft.json"
}
shot_f-agent-permission() {
  local d; d="$(copy proj fp)"
  shot f-agent-permission -- "$d/main.tex" --demo agent --agent-scenario "$agent/permission.json"
}
shot_f-inline-edit() {
  # The popover while the (sleeping) turn runs: the region, the instruction and the progress.
  local d; d="$(copy proj fi)"
  shot f-inline-edit LEMMA_DEMO_INLINE="Say that these notes test Lemma" LEMMA_DEMO_LINE=34 \
    LEMMA_DEMO_RETURN_MS=1500 -- "$d/main.tex" --demo agent --agent-scenario "$agent/cancel.json"
}
shot_g-completion() { shot g-completion -- --demo completion; }
shot_h-highlight-dark() { local d; d="$(copy proj h)"; shot h-highlight-dark -- "$d/chapters/yoneda.tex"; }
shot_h-highlight-light() { local d; d="$(copy proj hl)"; shot h-highlight-light "${light[@]}" -- "$d/chapters/yoneda.tex"; }

# ----- M3 -----
shot_i-outline() { local d; d="$(copy proj i)"; shot i-outline QUIT_MS=2500 -- "$d/main.tex" --demo outline; }
shot_j-hover-ref() { local d; d="$(copy proj j)"; shot j-hover-ref QUIT_MS=3000 -- "$d/main.tex" --demo hover-ref; }
# The same place without the hover: the inline equation numbers from main.aux.
shot_j-eqnumbers() {
  local d; d="$(copy proj je)"
  shot j-eqnumbers LEMMA_DEMO_HOVER=0 QUIT_MS=3000 -- "$d/main.tex" --demo hover-ref
}
# The tikz-cd diagram of chapters/yoneda.tex, rendered by pdflatex, in the hover.
shot_k-math-preview() {
  local d; d="$(copy proj k)"
  shot k-math-preview QUIT_MS=4000 -- "$d/chapters/yoneda.tex" --demo math-preview
}
shot_k-math-preview-light() {
  local d; d="$(copy proj kl)"
  shot k-math-preview-light "${light[@]}" QUIT_MS=4000 -- "$d/chapters/yoneda.tex" --demo math-preview
}
pdf50() { [ -f target/fixtures/pdf50.tex ] || scripts/build-fixtures.sh >/dev/null; }
shot_l-pdf-search() { pdf50; shot l-pdf-search QUIT_MS=4000 -- --demo pdf-search "$ROOT/target/fixtures/pdf50.tex"; }
shot_l-pdf-thumbs() {
  pdf50
  shot l-pdf-thumbs LEMMA_DEMO_PDF_QUERY= LEMMA_DEMO_PDF_STRIP=thumbnails QUIT_MS=4000 -- \
    --demo pdf-search "$ROOT/target/fixtures/pdf50.tex"
}
shot_l-pdf-bookmarks() {
  pdf50
  shot l-pdf-bookmarks LEMMA_DEMO_PDF_QUERY=lacus LEMMA_DEMO_PDF_STRIP=bookmarks QUIT_MS=4000 -- \
    --demo pdf-search "$ROOT/target/fixtures/pdf50.tex"
}
shot_m-palette() { local d; d="$(copy proj m)"; shot m-palette QUIT_MS=2500 -- "$d/main.tex" --demo palette; }
# Preferences needs a writable config.toml (with /dev/null every control is insensitive).
shot_m-prefs() {
  local d; d="$(copy proj mp)"
  : >"$work/mp-config.toml"
  shot m-prefs LEMMA_CONFIG="$work/mp-config.toml" QUIT_MS=2500 -- "$d/main.tex" --demo prefs
}
shot_n-search() { local d; d="$(copy proj n)"; shot n-search QUIT_MS=2500 -- "$d/main.tex" --demo search; }
shot_n-search-light() {
  local d; d="$(copy proj nl)"
  shot n-search-light "${light[@]}" QUIT_MS=2500 -- "$d/main.tex" --demo search
}
# A build error with an explanation and a quick fix: the Problems page shows the details of the
# first error of each kind (explanation, Apply Fix, Ask Agent).
shot_o-problems-explained() {
  local d; d="$(copy logs/missingdollar o)"
  shot o-problems-explained QUIT_MS=2500 -- "$d/main.tex" --build --sidebar problems
}
# A throwaway repository: proj committed, then main.tex edited since HEAD (changed, added and
# deleted lines) and one agent hunk, to compare the git strip with the review gutter.
shot_p-git-gutter() {
  local d; d="$(copy proj p)"
  local git_env=(GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_CEILING_DIRECTORIES="$work")
  if ! env "${git_env[@]}" GIT_AUTHOR_NAME=Lemma GIT_AUTHOR_EMAIL=lemma@example.invalid \
      GIT_COMMITTER_NAME=Lemma GIT_COMMITTER_EMAIL=lemma@example.invalid \
      sh -c 'git -C "$1" init -q -b main && git -C "$1" add . && git -C "$1" commit -q -m fixture' \
      sh "$d" >"$work/p-git.log" 2>&1; then
    echo "FAIL p-git-gutter.png: git init; output:"
    sed 's/^/     /' "$work/p-git.log"
    failures=$((failures + 1))
    return
  fi
  python3 - "$d/main.tex" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("\\usepackage{mathtools}\n", "\\usepackage{mathtools,bm}\n")
s = s.replace("\\usepackage{hyperref}\n",
              "\\usepackage{hyperref}\n\\usepackage{microtype}\n\\usepackage{cleveref}\n")
s = s.replace("\\newcommand{\\Sh}{\\operatorname{Sh}}\n", "")
open(p, "w").write(s)
PY
  cat >"$work/p.json" <<'JSON'
[{"tool": "edit", "file_path": "main.tex",
  "old_string": "\\title{Notes on Sheaves and Descent}",
  "new_string": "\\title{Sheaves, Descent and the Yoneda Lemma}"}]
JSON
  shot p-git-gutter "${light[@]}" "${git_env[@]}" QUIT_MS=3000 -- "$d/main.tex" \
    --apply-as-agent "$work/p.json"
}
shot_q-pairs() { local d; d="$(copy proj q)"; shot q-pairs QUIT_MS=3000 -- "$d/main.tex" --demo pairs; }
shot_q-pairs-light() {
  local d; d="$(copy proj ql)"; shot q-pairs-light "${light[@]}" QUIT_MS=3000 -- "$d/main.tex" --demo pairs
}

shot_b-review-turns() {
  rm -f "$OUT/b-review-turns.png"
  if LEMMA_TEST_SNAPSHOTS="$OUT" scripts/broadway.sh cargo test --quiet -p lemma-app \
      --test gtk_review_persist snapshot >"$work/turns.log" 2>&1 && [ -s "$OUT/b-review-turns.png" ]; then
    echo "ok   b-review-turns.png (tests/gtk_review_persist.rs)"
  else
    echo "FAIL b-review-turns.png; output (last 60 lines):"
    tail -n 60 "$work/turns.log" | sed 's/^/     /'
    failures=$((failures + 1))
  fi
}

# test_shots NAME TEST FILTER PNG...: runs the GTK test binary TEST (only the tests matching
# FILTER; "" runs all) with LEMMA_TEST_SNAPSHOTS=$OUT/tests and checks that it wrote every PNG.
# These are the M3 dialogs and states no command-line demo reaches.
test_shots() {
  local name="$1" test="$2" filter="$3" png missing=()
  shift 3
  mkdir -p "$OUT/tests"
  for png in "$@"; do rm -f "$OUT/tests/$png"; done
  local args=(--quiet -p lemma-app --test "$test")
  [ -n "$filter" ] && args+=(-- "$filter")
  if LEMMA_TEST_SNAPSHOTS="$OUT/tests" scripts/broadway.sh cargo test "${args[@]}" \
      >"$work/$name.log" 2>&1; then
    for png in "$@"; do [ -s "$OUT/tests/$png" ] || missing+=("$png"); done
  else
    missing=(the test failed)
  fi
  if [ ${#missing[@]} -eq 0 ]; then
    echo "ok   tests/{$(IFS=,; echo "$*")} (tests/$test.rs)"
  else
    echo "FAIL $name: ${missing[*]}; output (last 60 lines):"
    tail -n 60 "$work/$name.log" | sed 's/^/     /'
    failures=$((failures + 1))
  fi
}
shot_t-history-timeline() {
  test_shots t-history-timeline gtk_session timeline_lists_previews_and_restores \
    history-timeline-light.png history-timeline-dark.png
}
shot_t-lint() {
  test_shots t-lint gtk_lint lint_underline_is_soft_and_hovers \
    o-lint-hover-light.png o-lint-hover-dark.png o-problems-lint-light.png o-problems-lint-dark.png
}
shot_t-labels() {
  local kind scheme pngs=()
  for kind in j-references j-rename j-unused j-problems-references; do
    for scheme in light dark; do pngs+=("$kind-$scheme.png"); done
  done
  test_shots t-labels gtk_labels "" "${pngs[@]}"
}
shot_t-pairs() {
  test_shots t-pairs gtk_pairs tags_follow_the_style_scheme pairs-light.png pairs-dark.png
}
# M5: the template dialog (thesis selected), the environment tools and align helper, and the
# agent backend picker with the API key row and its dialog (a fake secret-tool).
shot_r-templates() { local d; d="$(copy proj r)"; shot r-templates QUIT_MS=2500 -- "$d/main.tex" --demo templates; }
shot_t-envtools() {
  test_shots t-envtools gtk_envtools "" envtools-change-env.png align-columns.png
}
shot_t-agent-backends() {
  test_shots t-agent-backends gtk_agent_backends api_key_row_and_dialog \
    r-api-key-row.png r-backend-picker.png r-api-key-dialog.png r-api-key-stored.png
}

all=(a-build a-build-light b-review-three b-review-all b-review-all-changes c-errors-editor
  c-errors-problems d-demo-pdf e-agent-then-build f-agent-panel f-agent-tools f-agent-permission
  f-inline-edit g-completion h-highlight-dark h-highlight-light b-review-turns
  i-outline j-hover-ref j-eqnumbers k-math-preview k-math-preview-light l-pdf-search l-pdf-thumbs
  l-pdf-bookmarks m-palette m-prefs n-search n-search-light o-problems-explained p-git-gutter
  q-pairs q-pairs-light t-history-timeline t-lint t-labels t-pairs r-templates t-envtools
  t-agent-backends)
names=("$@")
[ ${#names[@]} -gt 0 ] || names=("${all[@]}")
for n in "${names[@]}"; do
  n="${n%.png}"
  declare -F "shot_$n" >/dev/null || die "unknown shot $n (known: ${all[*]})"
  "shot_$n"
done

if [ "$failures" -gt 0 ]; then
  echo "shots.sh: $failures failure(s)"
  exit 1
fi
echo "shots.sh: ${#names[@]} shot(s) in $OUT"
