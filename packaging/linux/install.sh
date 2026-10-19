#!/bin/sh
# Installs Lemma from this folder (the Linux tarball of a release).
#
#   ./install.sh                     for you alone, into ~/.local
#   sudo ./install.sh --system       for everyone, into /usr/local
#   ./install.sh --prefix DIR        into DIR (bin/, share/)
#   ./install.sh --uninstall [--system | --prefix DIR]
#
# Lemma uses the system's GTK 4.22+, libadwaita 1.9+, GtkSourceView 5.18+, libspelling and
# poppler-glib 26.01+: it checks for them first and names the packages to install if they are
# missing.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
prefix="${HOME:-}/.local"
uninstall=""
force=""

die() {
  echo "install.sh: $*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --system) prefix=/usr/local ;;
    --prefix)
      [ $# -ge 2 ] || die "--prefix needs a directory"
      prefix="$2"
      shift
      ;;
    --prefix=*) prefix="${1#--prefix=}" ;;
    --uninstall) uninstall=1 ;;
    --force) force=1 ;;
    -h | --help)
      sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
  shift
done
case "$prefix" in /*) ;; *) die "--prefix needs an absolute path" ;; esac
manifest="$prefix/share/lemma/installed-files"

if [ -n "$uninstall" ]; then
  [ -f "$manifest" ] || die "Lemma was not installed into $prefix by this script"
  while IFS= read -r file; do
    rm -f "$file"
  done <"$manifest"
  rm -f "$manifest"
  rm -rf "$prefix/share/lemma/templates"
  rmdir "$prefix/share/lemma" 2>/dev/null || true
  command -v update-desktop-database >/dev/null &&
    update-desktop-database -q "$prefix/share/applications" 2>/dev/null || true
  echo "Lemma is removed from $prefix (your settings stay in ~/.config/lemma)."
  exit 0
fi

# The libraries: ldd names the missing ones; running lemma also catches a C library that is too
# old.
missing="$(ldd "$here/bin/lemma" 2>/dev/null | awk '/not found/ { print "  " $1 }')"
problem=""
if [ -n "$missing" ]; then
  problem="These libraries are missing:
$missing"
elif ! version_error="$("$here/bin/lemma" --version 2>&1 >/dev/null)"; then
  problem="Lemma does not start on this system:
  $version_error"
fi
if [ -n "$problem" ] && [ -z "$force" ]; then
  cat >&2 <<EOF
$problem

Lemma needs GTK 4.22+, libadwaita 1.9+, GtkSourceView 5.18+, libspelling and poppler-glib 26.01+
(the libraries of GNOME 50 or later). Install them with:

  Arch, Manjaro:      sudo pacman -S gtk4 libadwaita gtksourceview5 libspelling poppler-glib
  Fedora 44+:         sudo dnf install gtk4 libadwaita gtksourceview5 libspelling poppler-glib
  Ubuntu 26.04+, Debian sid:
                      sudo apt install libgtk-4-1 libadwaita-1-0 libgtksourceview-5-0 libspelling-1-2 libpoppler-glib8t64
  openSUSE Tumbleweed:
                      sudo zypper install libgtk-4-1 libadwaita-1-0 libgtksourceview-5-0 libspelling-1-2 libpoppler-glib8

Older releases (Debian 13, Ubuntu 25.10, Fedora 43 and earlier) do not have libraries this new.
Run ./install.sh --force to install anyway.
EOF
  exit 1
fi

mkdir -p "$prefix/share/lemma" || die "cannot write to $prefix (for --system, run it with sudo)"
: >"$manifest.new"
put() { # MODE SOURCE DEST
  install -Dm"$1" "$here/$2" "$prefix/$3"
  echo "$prefix/$3" >>"$manifest.new"
}
put 755 bin/lemma bin/lemma
put 755 bin/lemma-mcp bin/lemma-mcp
put 644 share/icons/hicolor/scalable/apps/io.github.lemma_editor.Lemma.svg \
  share/icons/hicolor/scalable/apps/io.github.lemma_editor.Lemma.svg
put 644 share/lemma/snippets.default.toml share/lemma/snippets.default.toml
put 644 share/lemma/prompts.default.toml share/lemma/prompts.default.toml
put 644 share/doc/lemma/README.md share/doc/lemma/README.md
if [ -d "$here/share/lemma/templates" ]; then
  rm -rf "$prefix/share/lemma/templates"
  cp -r "$here/share/lemma/templates" "$prefix/share/lemma/templates"
fi
# The launcher entry names the installed binary: ~/.local/bin is not always on a desktop's PATH.
desktop="share/applications/io.github.lemma_editor.Lemma.desktop"
mkdir -p "$prefix/share/applications"
sed "s|^Exec=lemma |Exec=$prefix/bin/lemma |" "$here/$desktop" >"$prefix/$desktop"
echo "$prefix/$desktop" >>"$manifest.new"
mv "$manifest.new" "$manifest"

command -v update-desktop-database >/dev/null &&
  update-desktop-database -q "$prefix/share/applications" 2>/dev/null || true
if [ -f "$prefix/share/icons/hicolor/index.theme" ] && command -v gtk-update-icon-cache >/dev/null; then
  gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" 2>/dev/null || true
fi

echo "Lemma $("$prefix/bin/lemma" --version 2>/dev/null | cut -d' ' -f2) is installed in $prefix."
case ":${PATH:-}:" in
  *":$prefix/bin:"*) echo "Start it from your applications, or run: lemma path/to/main.tex" ;;
  *) echo "Start it from your applications, or run: $prefix/bin/lemma path/to/main.tex
($prefix/bin is not on your PATH.)" ;;
esac
command -v latexmk >/dev/null ||
  echo "To build documents, install TeX with latexmk too (see the README, \"Requirements\")."
