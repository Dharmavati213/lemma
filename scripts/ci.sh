#!/usr/bin/env bash
# The checks every change must pass (docs/ARCHITECTURE.md §10.1):
#   1. cargo fmt --check
#   2. cargo clippy --workspace --all-targets -- -D warnings
#   3. cargo test --workspace, run under scripts/broadway.sh so GTK tests use the offscreen
#      Broadway display and never open windows on the desktop.
# Also syntax-checks the shell scripts and checks the generated completion tables. The tests run
# on a private Broadway display started here (LEMMA_BROADWAY_DISPLAY overrides it). Extra
# arguments go to `cargo test` (e.g. `--release`).
#
# Respects CARGO_TARGET_DIR; parallel agents set their own (target/agents/<wp>).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

step() { printf '\n==> %s\n' "$*"; }

# The packaging scripts too: nothing here runs them, and the release would be the first to.
step "bash -n scripts/*.sh packaging/*.sh packaging/*/*.sh"
for script in scripts/*.sh packaging/*.sh packaging/*/*.sh; do
  bash -n "$script"
done
# The SyncTeX parser's notice that the packages ship: every vendored file still has one.
sh packaging/synctex-notice.sh >/dev/null
# The linters, on a machine that has them (the Tests workflow installs both). actionlint also
# runs shellcheck on the workflows' scripts when it finds it.
if command -v shellcheck >/dev/null; then
  step "shellcheck scripts/*.sh packaging/*.sh packaging/*/*.sh"
  shellcheck -x scripts/*.sh packaging/*.sh packaging/*/*.sh
fi
if command -v actionlint >/dev/null; then
  step "actionlint .github/workflows/*.yml"
  actionlint .github/workflows/*.yml
fi

step "cargo fmt --check"
cargo fmt --all --check

step "xtask gen-completion --check" && cargo run -q -p xtask -- gen-completion --check

step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

# The GUI tests get a private Broadway display: windows of other processes on a shared display
# (the default :7) can take the keyboard focus from them. LEMMA_BROADWAY_DISPLAY overrides it.
if [ -z "${LEMMA_BROADWAY_DISPLAY:-}" ] && command -v gtk4-broadwayd >/dev/null; then
  # broadwayd puts its socket in GLib's runtime directory; without XDG_RUNTIME_DIR the daemon
  # and the tests get one of their own (see scripts/broadway.sh, which has the same lines).
  if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
    XDG_RUNTIME_DIR="${TMPDIR:-/tmp}/lemma-runtime-$(id -u)"
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    export XDG_RUNTIME_DIR
  fi
  for n in $(seq 100 199); do
    sock="$XDG_RUNTIME_DIR/broadway$((n + 1)).socket"
    [ -e "$sock" ] && continue
    port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
    gtk4-broadwayd ":$n" --address 127.0.0.1 --port "$port" >/dev/null 2>&1 &
    daemon=$!
    # Set at once, so that an interrupted wait does not leave the daemon running.
    trap 'kill "$daemon" 2>/dev/null || true; rm -f "$sock"' EXIT
    for _ in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done
    if [ -S "$sock" ] && kill -0 "$daemon" 2>/dev/null; then
      export LEMMA_BROADWAY_DISPLAY=":$n"
      break
    fi
    kill "$daemon" 2>/dev/null || true
    trap - EXIT
  done
fi

step "cargo test --workspace (Broadway ${LEMMA_BROADWAY_DISPLAY:-:7})"
# A GTK test program that finds no display fails here, where it would print SKIP and pass.
export LEMMA_TESTS_NEED_DISPLAY=1
scripts/broadway.sh cargo test --workspace "$@"

step "ci.sh: all checks passed"
