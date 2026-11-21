#!/bin/sh
# Prints the copyright and permission notice of the SyncTeX parser that Lemma compiles into its
# program on macOS and Windows and in the Linux packages built with LEMMA_SYNCTEX_BUNDLED=1
# (crates/lemma-synctex/vendor/synctex/). Its licence asks for the notice to go with every copy,
# so those packages ship this text beside Lemma's LICENSE (LICENSE.synctex).
#
#   packaging/synctex-notice.sh > LICENSE.synctex
#
# The notice is taken from the vendored files themselves, so it follows them when they are
# updated: each distinct notice is printed once, with the files that carry it. A vendored source
# without a notice is an error. POSIX sh (macOS's bundle.sh and MSYS2 run it too).
set -eu

dir="$(cd "$(dirname "$0")/.." && pwd)/crates/lemma-synctex/vendor/synctex"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

count=0
for file in "$dir"/*.c "$dir"/*.h; do
  [ -f "$file" ] || continue
  # The copyright line and the permission notice, without the comment's indentation (and
  # without the lines between them, which name the file's own revision).
  awk '{ sub(/^[ \t]+/, ""); sub(/[ \t\r]+$/, "") }
    !copyright && /^Copyright \(c\)/ { copyright = 1; print; print ""; next }
    copyright && /^Permission is hereby granted/ { body = 1 }
    body { print }
    body && /authorization from the copyright holder\.$/ { done = 1; exit }
    END { if (!done) exit 1 }' "$file" >"$tmp/notice" || {
    echo "synctex-notice.sh: no complete copyright notice in $file" >&2
    exit 1
  }
  same=""
  i=1
  while [ "$i" -le "$count" ]; do
    if cmp -s "$tmp/notice" "$tmp/notice.$i"; then
      same="$i"
      break
    fi
    i=$((i + 1))
  done
  if [ -z "$same" ]; then
    count=$((count + 1))
    same="$count"
    mv "$tmp/notice" "$tmp/notice.$same"
  fi
  basename "$file" >>"$tmp/files.$same"
done
[ "$count" -gt 0 ] || {
  echo "synctex-notice.sh: no sources in $dir" >&2
  exit 1
}

echo "Lemma's program contains the SyncTeX parser by Jérôme Laurens (TeX Live's"
echo "texk/web2c/synctexdir), compiled from these files under the notice below."
i=1
while [ "$i" -le "$count" ]; do
  echo
  echo "Files: $(tr '\n' ' ' <"$tmp/files.$i" | sed 's/ $//')"
  echo
  cat "$tmp/notice.$i"
  i=$((i + 1))
done
