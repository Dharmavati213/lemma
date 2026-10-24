#!/usr/bin/env bash
# Builds Lemma for Windows (x86_64) and packages it: the app folder dist/Lemma, a zip of it and
# an installer (see packaging/windows/README.md).
#
#   packaging/windows/build.sh [--skip-build] [--no-installer] [--test] [--smoke]
#
# Two ways, chosen by where it runs:
#
# - On Windows, in an MSYS2 CLANG64 shell: GTK and the rest come from MSYS2's packages, Rust
#   and clang from MSYS2 too, and Inno Setup (if installed) makes the installer.
# - On Linux, as a cross build: everything is downloaded once into the SDK directory (llvm-mingw,
#   MSYS2's CLANG64 packages, a portable Wine and Inno Setup, each checked against a pinned or
#   published SHA-256), and the build runs for the x86_64-pc-windows-gnullvm target. Wine runs
#   the few Windows tools the packaging needs (gdk-pixbuf-query-loaders, Inno Setup's compiler)
#   and, with --test and --smoke, the tests and the app. Needs rustup, curl, python3, tar with
#   zstd, pkg-config and glib-compile-schemas.
#
# Options:
#   --skip-build    package the existing release build
#   --no-installer  only the folder and the zip
#   --test          run the tests of the crates without GTK windows (on Linux under Wine)
#   --smoke         start the packaged app on a test project and save a snapshot of its
#                   window in dist/smoke (on Linux under Wine, on your desktop's display)
#
# Env: LEMMA_DIST_DIR      output directory (default: dist)
#      LEMMA_WINDOWS_NAME  file name of the zip and the installer without extension
#                          (default: Lemma-VERSION-windows-x86_64)
#      LEMMA_WINDOWS_SDK   the cross build's download directory (default: target/windows-sdk)
#      CARGO_TARGET_DIR    respected
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
HERE="$ROOT/packaging/windows"

step() { printf '\n==> %s\n' "$*"; }
die() { echo "build.sh: $*" >&2; exit 1; }

SKIP_BUILD=0 INSTALLER=1 TEST=0 SMOKE=0
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
    --no-installer) INSTALLER=0 ;;
    --test) TEST=1 ;;
    --smoke) SMOKE=1 ;;
    -h|--help) sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $arg (see --help)" ;;
  esac
done

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n 1)"
[ -n "$VERSION" ] || die "no version in Cargo.toml"
NAME="${LEMMA_WINDOWS_NAME:-Lemma-$VERSION-windows-x86_64}"
OUT="${LEMMA_DIST_DIR:-$ROOT/dist}"
case "$OUT" in /*) ;; *) OUT="$ROOT/$OUT" ;; esac
APP="$OUT/Lemma"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$ROOT/$TARGET_DIR" ;; esac

# The MSYS2 packages Lemma needs at run time (and to build against).
PACKAGES=(gtk4 libadwaita gtksourceview5 libspelling poppler glib2 pango cairo zlib librsvg
  adwaita-icon-theme hunspell-en gdk-pixbuf2 enchant fontconfig)

if [ -n "${MSYSTEM:-}" ]; then
  # ------------------------------------------------------------------ native, in MSYS2
  [ "$MSYSTEM" = CLANG64 ] || die "run this in MSYS2's CLANG64 shell (this is $MSYSTEM)"
  PREFIX="$MINGW_PREFIX"
  P="$MINGW_PACKAGE_PREFIX"
  missing=()
  for pkg in "${PACKAGES[@]}" rust pkgconf toolchain python; do
    pacman -Q "$P-$pkg" >/dev/null 2>&1 || pacman -Qg "$P-$pkg" >/dev/null 2>&1 \
      || missing+=("$P-$pkg")
  done
  [ ${#missing[@]} -eq 0 ] || die "missing MSYS2 packages; install them with:
  pacman -S --needed ${missing[*]}"
  RELEASE="$TARGET_DIR/release"
  CARGO_TARGET=()
  RUNNER=()
  win_path() { cygpath -w "$1"; }
  host_python=python3
else
  # ------------------------------------------------------------------ cross, from Linux
  [ "$(uname -s)" = Linux ] || die "run this on Linux, or on Windows in MSYS2's CLANG64 shell"
  for tool in rustup cargo curl python3 tar pkg-config glib-compile-schemas sha256sum; do
    command -v "$tool" >/dev/null || die "$tool is needed"
  done
  SDK="${LEMMA_WINDOWS_SDK:-$TARGET_DIR/windows-sdk}"
  case "$SDK" in /*) ;; *) SDK="$ROOT/$SDK" ;; esac
  mkdir -p "$SDK/downloads"

  # fetch NAME URL SHA256: a pinned download, checked.
  fetch() {
    local file="$SDK/downloads/$1"
    if [ ! -f "$file" ] || ! echo "$3  $file" | sha256sum -c --status; then
      echo "downloading $2"
      curl -fsSL --retry 3 -o "$file.part" "$2"
      echo "$3  $file.part" | sha256sum -c --status || die "$1 does not match its SHA-256"
      mv "$file.part" "$file"
    fi
  }
  LLVM_MINGW=llvm-mingw-20261020-ucrt-ubuntu-22.04-x86_64
  fetch "$LLVM_MINGW.tar.xz" \
    "https://github.com/mstorsjo/llvm-mingw/releases/download/20261020/$LLVM_MINGW.tar.xz" \
    55064b60b991a1e1e8090b8c2f84e82b22cfe5903d12c159d65064cf5e8ae335
  WINE_BUILD=wine-11.20-amd64
  fetch "$WINE_BUILD.tar.xz" \
    "https://github.com/Kron4ek/Wine-Builds/releases/download/11.20/$WINE_BUILD.tar.xz" \
    3fcc6202695f5277ec03653c8e284eac560b54947fa8518552f7c98619a4ac9d
  for archive in "$LLVM_MINGW" "$WINE_BUILD"; do
    [ -d "$SDK/$archive" ] || tar -xf "$SDK/downloads/$archive.tar.xz" -C "$SDK"
  done

  step "MSYS2 CLANG64 packages"
  mkdir -p "$SDK/msys2"
  python3 -I "$HERE/msys2.py" "$SDK/msys2" "${PACKAGES[@]/#/mingw-w64-clang-x86_64-}" \
    > "$SDK/msys2/fetched.txt"
  wc -l < "$SDK/msys2/fetched.txt" | sed 's/$/ packages/'
  PREFIX="$SDK/msys2/clang64"

  export PATH="$SDK/$LLVM_MINGW/bin:$PATH"
  export WINEPREFIX="$SDK/wineprefix" WINEDEBUG=-all WINEDLLOVERRIDES="mscoree,mshtml="
  WINE="$SDK/$WINE_BUILD/bin/wine"
  if [ ! -f "$WINEPREFIX/system.reg" ]; then
    echo "creating the Wine prefix $WINEPREFIX"
    "$WINE" wineboot -i >/dev/null 2>&1 || die "wine could not start"
    # Wine has none of Windows' fonts. Poppler draws a PDF's standard fonts that are not
    # embedded with Arial, Times New Roman and Courier New (a test needs it): the Liberation
    # fonts have their metrics, where the system has them.
    fonts="$WINEPREFIX/drive_c/windows/Fonts"
    for pair in arial:LiberationSans-Regular arialbd:LiberationSans-Bold \
      times:LiberationSerif-Regular timesbd:LiberationSerif-Bold cour:LiberationMono-Regular; do
      file="$(fc-list -f '%{file}\n' 2>/dev/null | grep -m 1 "/${pair#*:}\.ttf$" || true)"
      [ -z "$file" ] || cp "$file" "$fonts/${pair%%:*}.ttf"
    done
  fi
  # Windows programs from the bundle find their DLLs next to them; these find MSYS2's.
  export WINEPATH="$("$WINE" winepath -w "$PREFIX/bin" 2>/dev/null);$("$WINE" winepath -w "$SDK/$LLVM_MINGW/x86_64-w64-mingw32/bin" 2>/dev/null)"

  rustup target add x86_64-pc-windows-gnullvm >/dev/null
  TRIPLE=x86_64-pc-windows-gnullvm
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER=x86_64-w64-mingw32-clang
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUSTFLAGS="-L $PREFIX/lib"
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER="$WINE"
  export CC_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang
  export CXX_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang++
  export AR_x86_64_pc_windows_gnullvm=llvm-ar
  export WINDRES=x86_64-w64-mingw32-windres
  export PKG_CONFIG_ALLOW_CROSS=1
  export PKG_CONFIG_SYSROOT_DIR="$SDK/msys2"
  export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig:$PREFIX/share/pkgconfig"
  RELEASE="$TARGET_DIR/$TRIPLE/release"
  CARGO_TARGET=(--target "$TRIPLE")
  RUNNER=("$WINE")
  win_path() { "$WINE" winepath -w "$1" 2>/dev/null; }
  host_python=python3
fi

if [ "$SKIP_BUILD" = 0 ]; then
  step "cargo build --release (Windows x86_64)"
  cargo build --release --locked "${CARGO_TARGET[@]}" -p lemma-app -p lemma-mcp
fi
[ -f "$RELEASE/lemma.exe" ] && [ -f "$RELEASE/lemma-mcp.exe" ] \
  || die "no release build in $RELEASE"

TESTS_FAILED=0
if [ "$TEST" = 1 ]; then
  step "tests of the crates without GTK windows"
  THREADS=()
  if [ -z "${MSYSTEM:-}" ]; then
    # Wine starts and ends processes slowly, more so under load: the agent tests time their
    # shutdown, so the tests run one at a time.
    THREADS=(-- --test-threads=1)
    # Under Wine the tests cannot run cargo to build their helper programs, and insta cannot
    # ask cargo where the workspace is.
    cargo build --locked "${CARGO_TARGET[@]}" -p lemma-fake-agent -p lemma-mcp
    debug="$TARGET_DIR/$TRIPLE/debug"
    LEMMA_FAKE_AGENT="$(win_path "$debug/lemma-fake-agent.exe")"
    LEMMA_MCP_BRIDGE="$(win_path "$debug/lemma-mcp.exe")"
    INSTA_WORKSPACE_ROOT="$(win_path "$ROOT")"
    export LEMMA_FAKE_AGENT LEMMA_MCP_BRIDGE INSTA_WORKSPACE_ROOT
  fi
  # A failure is reported at the end; the app is packaged anyway.
  cargo test --locked --no-fail-fast "${CARGO_TARGET[@]}" \
    -p lemma-core -p lemma-config -p lemma-syntax -p lemma-snippets -p lemma-complete \
    -p lemma-review -p lemma-search -p lemma-build -p lemma-synctex -p lemma-mcp \
    -p lemma-agent -p lemma-pdf -p lemma-preview "${THREADS[@]}" || TESTS_FAILED=1
  cargo test --locked --no-fail-fast "${CARGO_TARGET[@]}" -p lemma-app --lib "${THREADS[@]}" \
    || TESTS_FAILED=1
fi

step "the app folder: $APP"
rm -rf "$APP"
mkdir -p "$APP/bin" "$APP/lib" "$APP/share" "$APP/etc"
# lemma finds lemma-mcp next to itself; GLib's spawn helpers run programs for GSubprocess.
cp "$RELEASE/lemma.exe" "$RELEASE/lemma-mcp.exe" "$APP/bin/"
cp "$PREFIX/bin/gspawn-win64-helper.exe" "$PREFIX/bin/gspawn-win64-helper-console.exe" "$APP/bin/"

# gdk-pixbuf's loaders, and its list of them: paths relative to the folder, so it stays valid
# wherever the folder goes (MSYS2's gdk-pixbuf is relocatable).
PIXBUF=lib/gdk-pixbuf-2.0/2.10.0
mkdir -p "$APP/$PIXBUF/loaders"
cp "$PREFIX/$PIXBUF/loaders/"*.dll "$APP/$PIXBUF/loaders/"
(cd "$PREFIX/bin" && "${RUNNER[@]}" ./gdk-pixbuf-query-loaders.exe) | tr -d '\r' \
  | grep -v '^# LoaderDir = ' > "$APP/$PIXBUF/loaders.cache"
grep -q '^"lib\\\\gdk-pixbuf-2.0' "$APP/$PIXBUF/loaders.cache" \
  || die "gdk-pixbuf-query-loaders wrote no relative loader paths"

# Spell checking: enchant with Hunspell (and the English dictionaries) and with Windows' own
# spell checker (winspell), which knows the languages installed in Windows.
mkdir -p "$APP/lib/enchant-2" "$APP/share/hunspell"
cp "$PREFIX/lib/enchant-2/enchant_hunspell.dll" "$PREFIX/lib/enchant-2/enchant_winspell.dll" \
  "$APP/lib/enchant-2/"
for dict in en_US en_GB; do
  cp "$PREFIX/share/hunspell/$dict.aff" "$PREFIX/share/hunspell/$dict.dic" "$APP/share/hunspell/"
done

step "libraries"
"$host_python" -I "$HERE/bundle_dlls.py" "$APP/bin" "$PREFIX/bin" \
  "$APP/bin/"*.exe "$APP/$PIXBUF/loaders/"*.dll "$APP/lib/enchant-2/"*.dll | wc -l \
  | sed 's/$/ DLLs/'

step "data"
mkdir -p "$APP/share/glib-2.0/schemas"
cp "$PREFIX/share/glib-2.0/schemas/"org.gtk.gtk4.*.gschema.xml "$APP/share/glib-2.0/schemas/"
glib-compile-schemas "$APP/share/glib-2.0/schemas"
mkdir -p "$APP/share/icons"
cp -R "$PREFIX/share/icons/Adwaita" "$PREFIX/share/icons/hicolor" "$APP/share/icons/"
mkdir -p "$APP/share/icons/hicolor/scalable/apps"
cp packaging/io.github.lemma_editor.Lemma.svg "$APP/share/icons/hicolor/scalable/apps/"
cp -R "$PREFIX/share/gtksourceview-5" "$APP/share/"
cp -R "$PREFIX/etc/fonts" "$APP/etc/"
# The defaults built into lemma, as examples for %APPDATA%\lemma.
mkdir -p "$APP/share/lemma"
cp data/snippets.default.toml data/prompts.default.toml "$APP/share/lemma/"
cp -R data/templates "$APP/share/lemma/templates"

step "licences"
mkdir -p "$APP/share/licenses/lemma"
cp LICENSE "$APP/share/licenses/lemma/LICENSE"
# The MSYS2 package of every file in the folder, and that package's licence files.
owners="$(mktemp)"
if [ -n "${MSYSTEM:-}" ]; then
  (cd "$APP" && find bin lib etc share -type f) | while read -r f; do
    [ -e "$PREFIX/$f" ] && printf '%s\n' "$(pacman -Qqo "$PREFIX/$f" 2>/dev/null)"
  done | sort -u > "$owners"
  pacman -Q $(cat "$owners") > "$APP/share/licenses/msys2-packages.txt"
else
  (cd "$APP" && find bin lib etc share -type f) | sed 's|^|clang64/|' \
    | awk -F'\t' 'NR == FNR { want[$0] = 1; next } ($1 in want) { print $2 }' - \
      "$SDK/msys2/owners.txt" | sort -u > "$owners"
  awk 'NR == FNR { want[$0] = 1; next } ($1 in want)' "$owners" "$SDK/msys2/packages.txt" \
    > "$APP/share/licenses/msys2-packages.txt"
fi
while read -r pkg; do
  short="${pkg#mingw-w64-clang-x86_64-}"
  [ -d "$PREFIX/share/licenses/$short" ] && cp -R "$PREFIX/share/licenses/$short" \
    "$APP/share/licenses/"
done < "$owners"
rm -f "$owners"
cat > "$APP/README.txt" <<EOF
Lemma $VERSION for Windows

Start bin\\lemma.exe (or install Lemma with the setup program, which adds it to the Start menu).
To build documents, install a TeX distribution: MiKTeX or TeX Live.
More: https://github.com/dharmavati213/lemma

Lemma is free software under the GNU General Public License v3.0 or later
(share\\licenses\\lemma\\LICENSE). It comes with GTK and other libraries from MSYS2
(share\\licenses\\msys2-packages.txt lists them; their licences are in share\\licenses).
EOF
sed -i 's/$/\r/' "$APP/README.txt"
du -sh "$APP" | cut -f1 | sed 's/^/size: /'

step "zip: $NAME.zip"
rm -f "$OUT/$NAME.zip"
"$host_python" -I - "$OUT" "$NAME.zip" <<'EOF'
import os, sys, zipfile
out, name = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(os.path.join(out, name), "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for root, dirs, files in os.walk(os.path.join(out, "Lemma")):
        dirs.sort()
        for f in sorted(files):
            path = os.path.join(root, f)
            z.write(path, os.path.relpath(path, out))
EOF

if [ "$INSTALLER" = 1 ]; then
  step "installer: $NAME-setup.exe"
  if [ -n "${MSYSTEM:-}" ]; then
    ISCC="$(command -v iscc || command -v ISCC || true)"
    for dir in "/c/Program Files (x86)/Inno Setup 6" "${LOCALAPPDATA:-}/Programs/Inno Setup 6"; do
      [ -z "$ISCC" ] && [ -x "$dir/ISCC.exe" ] && ISCC="$dir/ISCC.exe"
    done
    [ -n "$ISCC" ] || die "Inno Setup 6 is needed for the installer (https://jrsoftware.org); \
or pass --no-installer"
    ISCC=("$ISCC")
    # MSYS2 would take the /D options for paths and rewrite them.
    export MSYS2_ARG_CONV_EXCL='*'
  else
    fetch innosetup-6.7.3.exe \
      https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe \
      9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732
    if [ ! -f "$WINEPREFIX/drive_c/InnoSetup/ISCC.exe" ]; then
      "$WINE" "$SDK/downloads/innosetup-6.7.3.exe" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART \
        /CURRENTUSER '/DIR=C:\InnoSetup' >/dev/null 2>&1 || die "Inno Setup did not install"
    fi
    ISCC=("$WINE" 'C:\InnoSetup\ISCC.exe')
  fi
  "${ISCC[@]}" /Qp "/DVersion=$VERSION" "/DSourceDir=$(win_path "$APP")" \
    "/DOutputDir=$(win_path "$OUT")" "/DOutputName=$NAME-setup" \
    "/DIconFile=$(win_path "$HERE/lemma.ico")" "/DLicenseFile=$(win_path "$ROOT/LICENSE")" \
    "$(win_path "$HERE/lemma.iss")"
  [ -f "$OUT/$NAME-setup.exe" ] || die "Inno Setup made no installer"
fi

if [ "$SMOKE" = 1 ]; then
  step "smoke test: the packaged app on tests/fixtures/proj"
  smoke="$OUT/smoke"
  rm -rf "$smoke"
  mkdir -p "$smoke/state"
  cp -R tests/fixtures/proj "$smoke/proj"
  status=0
  # A Linux desktop's GDK_BACKEND would reach the program through Wine. (Wine itself needs the
  # desktop's display: the window opens there for a few seconds.)
  env -u GDK_BACKEND LEMMA_CONFIG=NUL LEMMA_LOG=lemma_app=info \
    XDG_STATE_HOME="$(win_path "$smoke/state")" \
    "${RUNNER[@]}" "$APP/bin/lemma.exe" "$(win_path "$smoke/proj/main.tex")" \
    --snapshot "$(win_path "$smoke/window.png")" --quit-after-ms 15000 \
    > "$smoke/window.log" 2>&1 || status=$?
  if [ "$status" = 0 ] && [ -s "$smoke/window.png" ]; then
    echo "ok   $smoke/window.png"
  else
    tail -n 40 "$smoke/window.log"
    die "the app did not start (exit $status); its output is in $smoke/window.log"
  fi
fi

step "done"
ls -l "$OUT/$NAME.zip"
[ "$INSTALLER" = 0 ] || ls -l "$OUT/$NAME-setup.exe"
[ "$TESTS_FAILED" = 0 ] || die "some tests failed (see above)"
