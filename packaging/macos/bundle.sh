#!/usr/bin/env bash
# Builds Lemma.app and a DMG for macOS from a release build and Homebrew's GTK stack
# (see packaging/macos/README.md).
#
#   packaging/macos/bundle.sh [--skip-build]
#
# Needs: macOS on Apple silicon, Rust, Xcode's command line tools, and
#   brew install gtk4 libadwaita gtksourceview5 libspelling poppler adwaita-icon-theme librsvg
#
# Env: LEMMA_DIST_DIR   output directory (default: dist)
#      LEMMA_DMG_NAME   the DMG's file name without .dmg (default: Lemma-VERSION-macos-ARCH)
#      CARGO_TARGET_DIR respected
#      MACOSX_DEPLOYMENT_TARGET  default: this Mac's major version, which Homebrew's bottles
#                       are built for (they are the floor anyway)
#
# The app is self-contained: GTK and every other library come along in
# Contents/Resources/lib, their data in Contents/Resources/share, and Lemma sets up the
# environment for them itself (crates/lemma-app/src/macos.rs). It is signed ad hoc only.
# Written for macOS's bash 3.2.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
HERE="$ROOT/packaging/macos"

step() { printf '\n==> %s\n' "$*"; }
die() { echo "bundle.sh: $*" >&2; exit 1; }

[ "$(uname -s)" = Darwin ] || die "this builds the macOS app; run it on a Mac"
command -v brew >/dev/null || die "Homebrew is needed (https://brew.sh)"
BREW="$(brew --prefix)"
for formula in gtk4 libadwaita gtksourceview5 libspelling poppler adwaita-icon-theme librsvg; do
  [ -d "$BREW/opt/$formula" ] || die "missing Homebrew formula: brew install $formula"
done

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n 1)"
[ -n "$VERSION" ] || die "no version in Cargo.toml"
ARCH="$(uname -m)"
MIN_MACOS="${MACOSX_DEPLOYMENT_TARGET:-$(sw_vers -productVersion | cut -d. -f1).0}"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$ROOT/$TARGET_DIR" ;; esac
OUT="${LEMMA_DIST_DIR:-$ROOT/dist}"
case "$OUT" in /*) ;; *) OUT="$ROOT/$OUT" ;; esac
DMG_NAME="${LEMMA_DMG_NAME:-Lemma-$VERSION-macos-$ARCH}"

APP="$OUT/Lemma.app"
CONTENTS="$APP/Contents"
RES="$CONTENTS/Resources"
LIB="$RES/lib"
SHARE="$RES/share"

if [ "${1:-}" != --skip-build ]; then
  step "cargo build --release (macOS $MIN_MACOS and later)"
  export MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS"
  # Room in the load commands for the bundle's longer library paths (install_name_tool).
  export CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS="-C link-arg=-Wl,-headerpad_max_install_names"
  export CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS="$CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS"
  cargo build --release --locked -p lemma-app -p lemma-mcp
fi
[ -x "$TARGET_DIR/release/lemma" ] || die "no release build in $TARGET_DIR/release"

step "Lemma.app $VERSION"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$LIB" "$SHARE"
# lemma finds lemma-mcp next to itself.
install -m 755 "$TARGET_DIR/release/lemma" "$TARGET_DIR/release/lemma-mcp" "$CONTENTS/MacOS/"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MIN_MACOS@/$MIN_MACOS/g" "$HERE/Info.plist.in" \
  > "$CONTENTS/Info.plist"
plutil -lint "$CONTENTS/Info.plist" >/dev/null
printf 'APPL????' > "$CONTENTS/PkgInfo"

step "icon"
iconset="$(mktemp -d)/lemma.iconset"
mkdir -p "$iconset"
svg="$ROOT/packaging/io.github.lemma_editor.Lemma.svg"
for size in 16 32 128 256 512; do
  rsvg-convert -w "$size" -h "$size" "$svg" -o "$iconset/icon_${size}x${size}.png"
  rsvg-convert -w $((size * 2)) -h $((size * 2)) "$svg" -o "$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns -o "$RES/lemma.icns" "$iconset"
rm -rf "$(dirname "$iconset")"

step "modules: gdk-pixbuf loaders, enchant providers"
shopt -s nullglob
modules=()
pixbuf_src="$BREW/lib/gdk-pixbuf-2.0/2.10.0/loaders"
pixbuf_dst="$LIB/gdk-pixbuf-2.0/2.10.0/loaders"
mkdir -p "$pixbuf_dst"
loaders=("$pixbuf_src"/*.so)
for loader in ${loaders[@]+"${loaders[@]}"}; do
  cp -L "$loader" "$pixbuf_dst/"
  modules+=("$pixbuf_dst/$(basename "$loader")")
done
# gdk-pixbuf wants absolute paths: Lemma writes the real list at start-up from this template.
if [ "${#loaders[@]}" -gt 0 ]; then
  "$BREW/bin/gdk-pixbuf-query-loaders" "${loaders[@]}" |
    sed -E 's|^"/[^"]*/(libpixbufloader[^/"]*\.so)"|"@RESOURCES@/lib/gdk-pixbuf-2.0/2.10.0/loaders/\1"|' \
    > "$LIB/gdk-pixbuf-2.0/2.10.0/loaders.cache.in"
  if grep -q "\"$BREW" "$LIB/gdk-pixbuf-2.0/2.10.0/loaders.cache.in"; then
    die "loaders.cache.in still names Homebrew's loaders"
  fi
fi
echo "${#loaders[@]} gdk-pixbuf loaders"
# Enchant (libspelling) loads its providers from lib/enchant-2 next to its own library.
mkdir -p "$LIB/enchant-2"
providers=("$BREW/lib/enchant-2"/*.so)
for provider in ${providers[@]+"${providers[@]}"}; do
  cp -L "$provider" "$LIB/enchant-2/"
  modules+=("$LIB/enchant-2/$(basename "$provider")")
done
echo "${#providers[@]} enchant providers: $(cd "$LIB/enchant-2" && echo *)"
if [ -d "$BREW/share/enchant-2" ]; then
  cp -RL "$BREW/share/enchant-2" "$SHARE/"
fi
# GIO and GTK modules come only from here (none are needed); see macos.rs.
mkdir -p "$LIB/gio/modules"

step "libraries"
python3 "$HERE/bundle_dylibs.py" "$APP" "$CONTENTS/MacOS/lemma" "$CONTENTS/MacOS/lemma-mcp" \
  ${modules[@]+"${modules[@]}"}

step "data: GSettings schemas, icon themes, GtkSourceView"
mkdir -p "$SHARE/glib-2.0/schemas"
cp "$BREW/share/glib-2.0/schemas"/org.gtk.gtk4.*.gschema.xml "$SHARE/glib-2.0/schemas/"
"$BREW/bin/glib-compile-schemas" "$SHARE/glib-2.0/schemas"
mkdir -p "$SHARE/icons/hicolor/scalable/apps"
cp -RL "$BREW/share/icons/Adwaita" "$SHARE/icons/"
cp "$BREW/share/icons/hicolor/index.theme" "$SHARE/icons/hicolor/"
cp "$svg" "$SHARE/icons/hicolor/scalable/apps/"
for theme in "$SHARE/icons/Adwaita" "$SHARE/icons/hicolor"; do
  "$BREW/bin/gtk4-update-icon-cache" -q -t -f "$theme"
done
cp -RL "$BREW/share/gtksourceview-5" "$SHARE/"
# The defaults built into lemma, as examples for ~/.config/lemma/ (as on Linux).
mkdir -p "$SHARE/lemma"
cp data/snippets.default.toml data/prompts.default.toml "$SHARE/lemma/"
if [ -d data/templates ]; then cp -R data/templates "$SHARE/lemma/templates"; fi
cp LICENSE "$RES/"

step "ad-hoc signature"
# Inside out: libraries and modules, the helper, then the app (which signs the main executable).
find "$CONTENTS" -type f \( -name '*.dylib' -o -name '*.so' \) -print0 |
  xargs -0 codesign --force --sign - --timestamp=none
codesign --force --sign - --timestamp=none "$CONTENTS/MacOS/lemma-mcp"
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --strict --deep "$APP"

step "smoke test: lemma --version"
"$CONTENTS/MacOS/lemma" --version

step "$DMG_NAME.dmg"
staging="$(mktemp -d)"
cp -R "$APP" "$staging/"
ln -s /Applications "$staging/Applications"
rm -f "$OUT/$DMG_NAME.dmg"
hdiutil create -quiet -volname "Lemma" -srcfolder "$staging" -fs HFS+ -format UDZO \
  -ov "$OUT/$DMG_NAME.dmg"
rm -rf "$staging"
du -sh "$APP" "$OUT/$DMG_NAME.dmg"
step "bundle.sh: done"
