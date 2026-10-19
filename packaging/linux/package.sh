#!/usr/bin/env bash
# Builds Lemma's Linux packages from a release build (see packaging/linux/README.md):
#
#   packaging/linux/package.sh deb|rpm|tarball [--build]
#
#   deb      lemma_<version>-1_<arch>.deb for Debian sid and Ubuntu 26.04 or later; the
#            dependencies come from dpkg-shlibdeps (run it on that system)
#   rpm      lemma-<version>-1.<dist>.<arch>.rpm for Fedora 44 or later; rpmbuild finds the
#            dependencies (run it on that system)
#   tarball  lemma-<label>-linux-<arch>.tar.gz with install.sh, for any distribution that has the
#            libraries (README, "Requirements")
#
# --build runs `cargo build --release` first, with the bundled SyncTeX (LEMMA_SYNCTEX_BUNDLED=1)
# so that the packages need no libsynctex. Otherwise the binaries must be in
# $CARGO_TARGET_DIR/release already.
#
# Env: LEMMA_DIST_DIR       output directory (default: dist)
#      LEMMA_RELEASE_LABEL  the tarball's name part (default: the version)
#      CARGO_TARGET_DIR     respected
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
HERE="$ROOT/packaging/linux"

die() { echo "package.sh: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

kind="${1:-}"
case "$kind" in deb | rpm | tarball) ;; *) die "usage: package.sh deb|rpm|tarball [--build]" ;; esac
shift
build=""
for arg in "$@"; do
  case "$arg" in --build) build=1 ;; *) die "unknown option: $arg" ;; esac
done

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n 1)"
[ -n "$VERSION" ] || die "no version in Cargo.toml"
LABEL="${LEMMA_RELEASE_LABEL:-$VERSION}"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$ROOT/$TARGET_DIR" ;; esac
OUT="${LEMMA_DIST_DIR:-$ROOT/dist}"
case "$OUT" in /*) ;; *) OUT="$ROOT/$OUT" ;; esac
SUMMARY="LaTeX editor that shows an AI agent's edits inline for review"
HOMEPAGE="https://github.com/Dharmavati213/lemma"
MAINTAINER="Dharmavati213 <327805552+Dharmavati213@users.noreply.github.com>"

if [ -n "$build" ]; then
  step "cargo build --release (bundled SyncTeX)"
  LEMMA_SYNCTEX_BUNDLED=1 cargo build --release --locked -p lemma-app -p lemma-mcp
fi
for bin in lemma lemma-mcp; do
  [ -x "$TARGET_DIR/release/$bin" ] || die "no $TARGET_DIR/release/$bin (build first, or --build)"
done
mkdir -p "$OUT"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# stage DEST PREFIX LICENSE_PATH: Lemma's files under DEST/PREFIX (the layout of the PKGBUILD).
stage() {
  local dest="$1$2"
  install -Dm755 "$TARGET_DIR/release/lemma" "$dest/bin/lemma"
  # lemma finds lemma-mcp next to itself.
  install -Dm755 "$TARGET_DIR/release/lemma-mcp" "$dest/bin/lemma-mcp"
  install -Dm644 packaging/io.github.lemma_editor.Lemma.desktop \
    "$dest/share/applications/io.github.lemma_editor.Lemma.desktop"
  install -Dm644 packaging/io.github.lemma_editor.Lemma.svg \
    "$dest/share/icons/hicolor/scalable/apps/io.github.lemma_editor.Lemma.svg"
  # The defaults built into lemma, as examples for ~/.config/lemma/.
  install -Dm644 -t "$dest/share/lemma" data/snippets.default.toml data/prompts.default.toml
  if [ -d data/templates ]; then
    cp -r data/templates "$dest/share/lemma/templates"
    chmod -R u=rwX,go=rX "$dest/share/lemma/templates"
  fi
  install -Dm644 README.md "$dest/share/doc/lemma/README.md"
  install -Dm644 LICENSE "$dest/$3"
}

deb() {
  if ! command -v dpkg-deb >/dev/null || ! command -v dpkg-shlibdeps >/dev/null; then
    die "deb needs dpkg-dev (Debian or Ubuntu)"
  fi
  local root="$work/root" arch deps size file
  stage "$root" /usr share/doc/lemma/copyright
  # dpkg-shlibdeps reads debian/control of a source package: give it a minimal one.
  mkdir -p "$work/src/debian"
  printf 'Source: lemma\n\nPackage: lemma\nArchitecture: any\n' >"$work/src/debian/control"
  deps="$(cd "$work/src" &&
    dpkg-shlibdeps -O -e"$root/usr/bin/lemma" -e"$root/usr/bin/lemma-mcp" |
    sed -n 's/^shlibs:Depends=//p')"
  [ -n "$deps" ] || die "dpkg-shlibdeps found no dependencies"
  arch="$(dpkg --print-architecture)"
  size="$(du -sk "$root" | cut -f1)"
  mkdir -p "$root/DEBIAN"
  cat >"$root/DEBIAN/control" <<EOF
Package: lemma
Version: $VERSION-1
Architecture: $arch
Maintainer: $MAINTAINER
Installed-Size: $size
Depends: $deps
Recommends: latexmk, biber, texlive-latex-recommended, texlive-bibtex-extra, hunspell-en-us | hunspell-dictionary
Suggests: chktex, texlive-extra-utils, texlive-latex-extra, git, nodejs, npm
Section: editors
Priority: optional
Homepage: $HOMEPAGE
Description: $SUMMARY
 Lemma is a GTK 4 LaTeX editor built around reviewing an AI agent's edits:
 every change the agent makes shows up inline as a hunk that you accept or
 reject on its own. It is also a complete LaTeX editor without the agent:
 continuous PDF preview with SyncTeX, build diagnostics in plain English,
 completion, snippets and a project outline.
EOF
  file="$OUT/lemma_${VERSION}-1_${arch}.deb"
  dpkg-deb --root-owner-group -Zxz --build "$root" "$file"
  echo "Depends: $deps"
  echo "$file"
}

rpm() {
  command -v rpmbuild >/dev/null || die "rpm needs rpmbuild (Fedora: dnf install rpm-build)"
  local root="$work/root"
  stage "$root" /usr share/licenses/lemma/LICENSE
  rpmbuild -bb --quiet \
    --define "_topdir $work/rpmbuild" \
    --define "lemma_version $VERSION" \
    --define "lemma_stage $root" \
    "$HERE/lemma.spec"
  find "$work/rpmbuild/RPMS" -name '*.rpm' -exec cp {} "$OUT/" \; -exec basename {} \;
}

tarball() {
  local name dir
  name="lemma-$LABEL-linux-$(uname -m)"
  dir="$work/$name"
  stage "$dir" "" LICENSE
  install -Dm755 "$HERE/install.sh" "$dir/install.sh"
  tar -C "$work" -czf "$OUT/$name.tar.gz" "$name"
  echo "$OUT/$name.tar.gz"
}

step "$kind (Lemma $VERSION)"
"$kind"
