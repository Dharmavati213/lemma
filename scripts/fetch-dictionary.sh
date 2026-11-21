#!/usr/bin/env bash
# Fetches the en_US hunspell dictionary for the tests that wait for spell checking, on a machine
# with no English dictionary installed (they print SKIP without one; see docs/testing.md).
#   scripts/fetch-dictionary.sh
# The files go to target/dictionaries/hunspell (CARGO_TARGET_DIR is followed), and
# scripts/broadway.sh gives that directory to enchant when the machine has no en_US dictionary
# of its own. Nothing outside target/ changes. The dictionary is the one LibreOffice ships (from
# SCOWL, http://wordlist.sourceforge.net; its licence is the README fetched beside it), at a
# fixed release, checked against the checksums below. Needs curl and sha256sum.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=libreoffice-26.8.2.2
BASE="https://raw.githubusercontent.com/LibreOffice/dictionaries/$TAG/en"
DIR="${CARGO_TARGET_DIR:-target}/dictionaries/hunspell"
mkdir -p "$DIR"
fetch() {
  local name="$1" sum="$2"
  if [ -f "$DIR/$name" ] && echo "$sum  $DIR/$name" | sha256sum --check --status; then
    echo "ok   $DIR/$name"
    return
  fi
  curl -fsSL --retry 3 -o "$DIR/$name.part" "$BASE/$name"
  if ! echo "$sum  $DIR/$name.part" | sha256sum --check --status; then
    rm -f "$DIR/$name.part"
    echo "fetch-dictionary.sh: $BASE/$name is not the file expected (checksum)" >&2
    exit 1
  fi
  mv "$DIR/$name.part" "$DIR/$name"
  echo "new  $DIR/$name"
}
fetch en_US.aff e746c882dd6f303c2c46e7452804b9201115a6942cfeb15f18f8edf774d2e24e
fetch en_US.dic f0b1a234bd178bdd01875b2a392a9647f888b8fe879f79c52aae62c2759b3647
fetch README_en_US.txt 168b4c01cb841f766a72f310b065c04d09cff46376c37a81d799593ce751c371
