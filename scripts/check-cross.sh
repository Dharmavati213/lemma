#!/usr/bin/env bash
# Lints the Windows and macOS code on Linux: `cargo clippy --workspace --all-targets` for
# x86_64-pc-windows-msvc and aarch64-apple-darwin, with warnings as errors. It finds what does
# not compile on those systems (code behind `cfg(windows)`, `cfg(target_os = "macos")`); it
# builds no program and runs no test, so the release workflows still have the last word.
#
#   rustup target add x86_64-pc-windows-msvc aarch64-apple-darwin    (once)
#   scripts/check-cross.sh [target...]
#
# Nothing is linked, so two things stand in for a real cross toolchain:
# - the C compiler and the archiver of the other system (ring, the bundled SyncTeX parser) are a
#   script that only creates the files it is asked for;
# - pkg-config answers with this machine's libraries (GTK and the others): their `-sys` crates
#   only need the versions.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
  targets=(x86_64-pc-windows-msvc aarch64-apple-darwin)
fi

tools="$(mktemp -d)"
trap 'rm -rf "$tools"' EXIT
cat > "$tools/stand-in" <<'STUB'
#!/bin/sh
# Creates the output files a compiler or an archiver was asked for, and nothing else.
prev=
for a in "$@"; do
  case "$prev" in -o|-Fo|cq|crs|rcs|cr) : > "$a" 2>/dev/null ;; esac
  case "$a" in
    -Fo*|/Fo*) f="${a#?Fo}"; [ -n "$f" ] && : > "$f" 2>/dev/null ;;
    -out:*|/OUT:*|-OUT:*|/out:*) : > "${a#*:}" 2>/dev/null ;;
  esac
  prev="$a"
done
exit 0
STUB
chmod +x "$tools/stand-in"

installed="$(rustup target list --installed 2>/dev/null || true)"
for target in "${targets[@]}"; do
  if ! grep -qx "$target" <<<"$installed"; then
    echo "SKIP: $target is not installed (rustup target add $target)"
    continue
  fi
  printf '\n==> cargo clippy --target %s\n' "$target"
  name="${target//-/_}"
  env PKG_CONFIG_ALLOW_CROSS=1 \
    "CC_$name=$tools/stand-in" "CXX_$name=$tools/stand-in" "AR_$name=$tools/stand-in" \
    cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done

printf '\n==> check-cross.sh: done\n'
