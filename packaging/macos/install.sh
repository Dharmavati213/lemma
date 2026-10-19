#!/bin/bash
# Installs Lemma on a Mac, and TeX with everything Lemma uses (latexmk, biber, chktex, texcount,
# latexindent, mylatexformat and the common LaTeX packages).
#
#   /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Dharmavati213/lemma/main/packaging/macos/install.sh)"
#
# Options (after `bash -s --` when piping the script, or as the environment variables in
# brackets):
#   --tex recommended   BasicTeX + Lemma's tools + the common packages (about 0.6 GB) (LEMMA_TEX)
#   --tex full          the whole MacTeX (about 7 GB download)
#   --tex none          no TeX
#   --from-source       build Lemma here instead of downloading it          (LEMMA_FROM_SOURCE=1)
#   --dmg FILE          install Lemma from this DMG                          (LEMMA_DMG)
#   --release TAG       this release instead of the latest one               (LEMMA_RELEASE)
#   --yes               ask nothing; take the defaults                       (LEMMA_YES=1)
#   --uninstall         remove Lemma (settings and TeX stay)
#
# It needs a Mac with Apple silicon and macOS 15 or later, and an administrator account for TeX.
# Lemma goes to /Applications (else ~/Applications), a `lemma` command to /usr/local/bin, TeX to
# /usr/local/texlive (on PATH through /Library/TeX/texbin). Written for macOS's bash 3.2.
# Everything runs from `main` at the end, so a partly downloaded script does nothing.
set -euo pipefail

REPO="${LEMMA_REPO:-Dharmavati213/lemma}"
SOURCE_REF="${LEMMA_SOURCE_REF:-main}"
DMG_ASSET="Lemma-macos-arm64.dmg"
CTAN="${LEMMA_CTAN_MIRROR:-https://mirror.ctan.org}"
TEX_BIN="${LEMMA_TEX_BIN:-/Library/TeX/texbin}"
COMMAND_PATH="${LEMMA_COMMAND_PATH:-/usr/local/bin/lemma}"
COMMAND_MARK="# Starts Lemma (written by Lemma's install script)."
MIN_MACOS=15
MIN_RUST_MINOR=92

# Lemma's own tools: builds, bibliographies, lint, word count, formatting, fast math previews.
TEX_TOOLS="latexmk biber biblatex logreq chktex texcount latexindent mylatexformat"
# The packages most documents need, on top of BasicTeX.
TEX_COMMON="collection-latexrecommended collection-mathscience collection-pictures
collection-fontsrecommended csquotes cleveref enumitem thmtools todonotes cancel multirow
wrapfig fancyhdr titlesec setspace lipsum algorithms algorithmicx physics braket stmaryrd rsfs
doublestroke esint bbm-macros mathdots tcolorbox environ trimspaces placeins float caption
subfig appendix tocloft soul ulem comment framed mdframed zref needspace siunitx listings
glossaries imakeidx makecell threeparttable adjustbox collectbox standalone pdfpages pdflscape
changepage ifoddpage xstring datetime2 tracklang fmtcount koma-script preprint orcidlink
bookmark etoc chngcntr extsizes anyfontsize newtx tex-gyre libertine xkeyval kvoptions"

TEX_CHOICE="${LEMMA_TEX:-}"
FROM_SOURCE="${LEMMA_FROM_SOURCE:-}"
DMG="${LEMMA_DMG:-}"
RELEASE="${LEMMA_RELEASE:-}"
YES="${LEMMA_YES:-${NONINTERACTIVE:-}}"
UNINSTALL=""
WORK=""
SUDO_KEEPALIVE=""
APP=""
NOTES=""

# ---------- output ----------

if [ -t 1 ]; then
  BOLD=$'\033[1m' RED=$'\033[31m' GREEN=$'\033[32m' YELLOW=$'\033[33m' OFF=$'\033[0m'
else
  BOLD="" RED="" GREEN="" YELLOW="" OFF=""
fi
step() { printf '\n%s==> %s%s\n' "$BOLD" "$*" "$OFF"; }
info() { printf '    %s\n' "$*"; }
ok() { printf '    %s✓%s %s\n' "$GREEN" "$OFF" "$*"; }
warn() { printf '    %s! %s%s\n' "$YELLOW" "$*" "$OFF" >&2; }
die() {
  printf '\n%sinstall.sh: %s%s\n' "$RED" "$*" "$OFF" >&2
  exit 1
}
note() { NOTES="${NOTES}  - $*"$'\n'; }

usage() {
  cat <<'USAGE'
Installs Lemma on a Mac, and TeX with everything Lemma uses.

  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Dharmavati213/lemma/main/packaging/macos/install.sh)"
  curl -fsSL …/install.sh | bash -s -- [options]

  --tex recommended   BasicTeX, Lemma's tools and the common packages (default, about 0.6 GB)
  --tex full          all of MacTeX (7 GB download)
  --tex none          no TeX
  --from-source       build Lemma here instead of downloading it
  --dmg FILE          install Lemma from this DMG
  --release TAG       this release instead of the latest one
  --yes               ask nothing; take the defaults
  --uninstall         remove Lemma (settings and TeX stay)

Environment: LEMMA_TEX, LEMMA_FROM_SOURCE=1, LEMMA_DMG, LEMMA_RELEASE, LEMMA_YES=1.
USAGE
}

# ---------- questions ----------

# True when questions can be asked (a terminal, and no --yes).
interactive() {
  [ -z "$YES" ] && [ -r /dev/tty ] && (: </dev/tty) 2>/dev/null
}

# ask QUESTION DEFAULT: the answer (read from the terminal, so `curl … | bash` works too).
ask() {
  local answer=""
  if interactive; then
    printf '%s%s%s ' "$BOLD" "$1" "$OFF" >/dev/tty
    IFS= read -r answer </dev/tty || answer=""
  fi
  printf '%s\n' "${answer:-$2}"
}

# confirm QUESTION: yes unless the answer starts with n.
confirm() {
  case "$(ask "$1 [Y/n]" y)" in [nN]*) return 1 ;; *) return 0 ;; esac
}

# ---------- administrator rights ----------

is_admin() {
  local groups
  groups=" $(id -Gn) "
  case "$groups" in *" admin "*) return 0 ;; *) return 1 ;; esac
}

need_sudo() {
  [ -n "$SUDO_KEEPALIVE" ] && return 0
  is_admin || die "installing TeX needs an administrator account (or run with --tex none)"
  if ! sudo -n true 2>/dev/null; then
    info "Your password is needed to install into /usr/local and /Library/TeX:"
    sudo -v || die "sudo failed"
  fi
  # TeX can take longer than sudo's five minutes: keep the ticket fresh while this runs.
  (while kill -0 "$$" 2>/dev/null; do
    sudo -n true 2>/dev/null
    sleep 50
  done) &
  SUDO_KEEPALIVE=$!
}

cleanup() {
  [ -n "$SUDO_KEEPALIVE" ] && kill "$SUDO_KEEPALIVE" 2>/dev/null
  [ -n "$WORK" ] && rm -rf "$WORK"
  return 0
}

# ---------- checks ----------

check_mac() {
  [ "$(uname -s)" = Darwin ] || die "this installs Lemma on macOS; on Linux see the README"
  [ "$(id -u)" -ne 0 ] || die "run it as yourself, not with sudo (it asks for your password)"
  [ "$(sysctl -in hw.optional.arm64)" = 1 ] ||
    die "Lemma for macOS needs a Mac with Apple silicon (M1 or newer)"
  local major
  major="$(sw_vers -productVersion | cut -d. -f1)"
  [ "$major" -ge "$MIN_MACOS" ] ||
    die "Lemma needs macOS $MIN_MACOS or later (this Mac has $(sw_vers -productVersion))"
}

# ---------- downloads ----------

# fetch URL FILE: downloads with a progress bar; prints the HTTP status (000: no connection).
fetch() {
  local code
  code="$(curl -L --retry 3 --retry-delay 2 -C - -# -o "$2" -w '%{http_code}' "$1")" || true
  printf '%s\n' "${code:-000}"
}

# check_package PKG: macOS installer packages must carry a signature Apple issued.
check_package() {
  local status
  status="$(pkgutil --check-signature "$1" 2>&1)" || die "$(basename "$1") is not signed"
  grep -q "signed by a developer certificate issued by Apple" <<<"$status" ||
    die "$(basename "$1") does not carry a developer signature from Apple"
  ok "$(basename "$1"): $(printf '%s\n' "$status" | sed -n 's/^ *1\. //p' | head -n 1)"
}

# ---------- Lemma ----------

app_dir() {
  if [ -n "${LEMMA_APP_DIR:-}" ]; then
    printf '%s\n' "$LEMMA_APP_DIR"
  elif [ -w /Applications ]; then
    printf '/Applications\n'
  else
    printf '%s/Applications\n' "$HOME"
  fi
}

# install_app SRC: copies SRC (a Lemma.app) into place and checks that it starts.
install_app() {
  local dir
  dir="$(app_dir)"
  mkdir -p "$dir"
  rm -rf "$dir/Lemma.app.new"
  ditto "$1" "$dir/Lemma.app.new"
  rm -rf "$dir/Lemma.app" || die "cannot replace $dir/Lemma.app (quit Lemma, or remove it)"
  mv "$dir/Lemma.app.new" "$dir/Lemma.app"
  APP="$dir/Lemma.app"
  # Downloads with curl carry no quarantine flag; a copy from elsewhere might.
  xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "$APP" 2>/dev/null || true
  "$APP/Contents/MacOS/lemma" --version >/dev/null || die "the installed Lemma does not start"
  ok "$("$APP/Contents/MacOS/lemma" --version) in $APP"
}

install_from_dmg() {
  local mnt
  mnt="$(mktemp -d "$WORK/mnt.XXXXXX")"
  hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mnt" "$1" >/dev/null ||
    die "cannot open $1"
  if [ ! -d "$mnt/Lemma.app" ]; then
    hdiutil detach "$mnt" -quiet || true
    die "$1 holds no Lemma.app"
  fi
  install_app "$mnt/Lemma.app"
  hdiutil detach "$mnt" -quiet || hdiutil detach "$mnt" -force -quiet || true
}

# Downloads the release's DMG into $WORK; false when there is no such release.
download_release() {
  local url code
  if [ -n "$RELEASE" ]; then
    url="https://github.com/$REPO/releases/download/$RELEASE/$DMG_ASSET"
  else
    url="https://github.com/$REPO/releases/latest/download/$DMG_ASSET"
  fi
  info "$url"
  code="$(fetch "$url" "$WORK/$DMG_ASSET")"
  case "$code" in
    200) return 0 ;;
    404) return 1 ;;
    000) die "no connection to github.com" ;;
    *) die "downloading Lemma failed (HTTP $code)" ;;
  esac
}

brew_bin() {
  local b
  for b in "$(command -v brew 2>/dev/null || true)" /opt/homebrew/bin/brew /usr/local/bin/brew; do
    [ -n "$b" ] && [ -x "$b" ] && { printf '%s\n' "$b"; return 0; }
  done
  return 1
}

# Builds Lemma.app from the sources (Xcode's command line tools, Homebrew and Rust as needed).
build_from_source() {
  step "Building Lemma from source (15 to 30 minutes)"
  if ! xcode-select -p >/dev/null 2>&1; then
    info "Installing Xcode's command line tools: click Install in the dialog that opens."
    xcode-select --install 2>/dev/null || true
    until xcode-select -p >/dev/null 2>&1; do sleep 10; done
    ok "Xcode command line tools"
  fi
  local brew
  if ! brew="$(brew_bin)"; then
    info "Installing Homebrew (https://brew.sh)"
    need_sudo
    NONINTERACTIVE=1 /bin/bash -c \
      "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    brew="$(brew_bin)" || die "Homebrew did not install"
  fi
  eval "$("$brew" shellenv)"
  info "Homebrew: GTK and the other libraries"
  HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1 brew install gtk4 libadwaita gtksourceview5 \
    libspelling poppler adwaita-icon-theme librsvg pkgconf
  # shellcheck disable=SC1091 # rustup's environment, when there is one
  if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
  if ! command -v cargo >/dev/null; then
    info "Homebrew: Rust"
    brew install rust
  fi
  local minor
  minor="$(rustc --version | sed -E 's/^rustc 1\.([0-9]+).*/\1/')"
  if [ "${minor:-0}" -lt "$MIN_RUST_MINOR" ]; then
    if command -v rustup >/dev/null; then rustup update stable; else brew upgrade rust; fi
  fi
  local src="${LEMMA_SOURCE_DIR:-$HOME/Library/Caches/lemma-install/src}"
  if [ -d "$src/.git" ]; then
    info "Updating the sources in $src"
    git -C "$src" fetch --depth 1 origin "$SOURCE_REF"
    git -C "$src" checkout -q -f FETCH_HEAD
  else
    info "Downloading the sources to $src"
    mkdir -p "$(dirname "$src")"
    git clone --depth 1 --branch "$SOURCE_REF" "https://github.com/$REPO.git" "$src"
  fi
  (cd "$src" && LEMMA_DIST_DIR="$src/dist" packaging/macos/bundle.sh)
  install_app "$src/dist/Lemma.app"
}

install_lemma() {
  step "Lemma"
  if [ -n "$DMG" ]; then
    install_from_dmg "$DMG"
  elif [ -n "$FROM_SOURCE" ]; then
    build_from_source
  elif download_release; then
    install_from_dmg "$WORK/$DMG_ASSET"
  else
    warn "No ready-made Lemma for macOS is published${RELEASE:+ as $RELEASE} yet."
    info "It can be built here instead: that installs Xcode's command line tools, Homebrew and"
    info "Rust if they are missing, and takes 15 to 30 minutes."
    confirm "Build Lemma from source?" || die "nothing to install; see https://github.com/$REPO"
    build_from_source
  fi
}

install_command() {
  local script
  script="$(printf '#!/bin/sh\n%s\nexec "%s/Contents/MacOS/lemma" "$@"\n' "$COMMAND_MARK" "$APP")"
  if [ -e "$COMMAND_PATH" ] && ! grep -qF "$COMMAND_MARK" "$COMMAND_PATH"; then
    warn "$COMMAND_PATH exists and is not Lemma's; the lemma command was not installed"
    return 0
  fi
  if [ -w "$(dirname "$COMMAND_PATH")" ]; then
    printf '%s\n' "$script" >"$COMMAND_PATH"
    chmod 755 "$COMMAND_PATH"
  elif is_admin; then
    need_sudo
    sudo mkdir -p "$(dirname "$COMMAND_PATH")"
    printf '%s\n' "$script" | sudo tee "$COMMAND_PATH" >/dev/null
    sudo chmod 755 "$COMMAND_PATH"
  else
    note "the lemma command needs an administrator; start Lemma from Applications instead"
    return 0
  fi
  ok "the lemma command: $COMMAND_PATH"
}

# ---------- TeX ----------

# Root's own HOME (-H): macOS's sudo keeps the user's, and tlmgr would leave root's files there.
tlmgr() { sudo -H "$TEX_BIN/tlmgr" "$@"; }

# The packages of LIST (words) that TeX Live does not have installed.
missing_packages() {
  local installed p out=""
  installed="$("$TEX_BIN/tlmgr" info --only-installed --data name 2>/dev/null)" || installed=""
  for p in $1; do
    grep -qx "$p" <<<"$installed" || out="$out $p"
  done
  printf '%s\n' "${out# }"
}

# Adds the packages of LIST that are missing (tlmgr, which needs a current TeX Live).
tlmgr_add() {
  local missing
  missing="$(missing_packages "$1")"
  if [ -z "$missing" ]; then
    ok "every package is there"
    return 0
  fi
  need_sudo
  info "tlmgr: $(printf '%s\n' "$missing" | wc -w | tr -d ' ') packages"
  if ! tlmgr update --self; then
    warn "tlmgr cannot update itself. TeX Live older than the current release cannot get packages:"
    warn "install the current MacTeX, or BasicTeX, from https://tug.org/mactex/"
    note "the TeX packages were not installed (old TeX Live)"
    return 0
  fi
  # shellcheck disable=SC2086 # a list of package names
  if ! tlmgr install $missing; then
    missing="$(missing_packages "$missing")"
    if [ -n "$missing" ]; then
      warn "some packages failed; trying them once more"
      # shellcheck disable=SC2086
      tlmgr install $missing || note "TeX packages that did not install: $missing"
    fi
  fi
  "$TEX_BIN/mktexlsr" >/dev/null 2>&1 || true
}

install_texlive_package() { # NAME FILE: BasicTeX.pkg or MacTeX.pkg
  local pkg="$WORK/$2" code
  info "$CTAN/systems/mac/mactex/$2"
  code="$(fetch "$CTAN/systems/mac/mactex/$2" "$pkg")"
  [ "$code" = 200 ] || die "downloading $2 failed (HTTP $code)"
  check_package "$pkg"
  need_sudo
  info "Installing $1 (a few minutes)"
  sudo -H installer -pkg "$pkg" -target / >/dev/null
  rm -f "$pkg"
  [ -x "$TEX_BIN/pdflatex" ] || die "$1 did not install"
  ok "$1: $("$TEX_BIN/pdflatex" --version | head -n 1)"
}

# Perl modules latexindent needs (macOS's Perl does not have them).
fix_latexindent() {
  local li="$TEX_BIN/latexindent"
  [ -x "$li" ] || return 0
  if "$li" --version >/dev/null 2>&1; then
    ok "latexindent works"
    return 0
  fi
  if ! xcode-select -p >/dev/null 2>&1; then
    note "formatting with latexindent needs Perl modules: run xcode-select --install, then this script again"
    return 0
  fi
  info "latexindent: Perl modules YAML::Tiny and File::HomeDir (CPAN)"
  need_sudo
  local perl log="$HOME/Library/Logs/lemma-install-cpan.log"
  perl="$(command -v perl)"
  mkdir -p "$(dirname "$log")"
  # The log is the user's file: the redirection is meant to happen outside sudo.
  # shellcheck disable=SC2024
  sudo -H env PERL_MM_USE_DEFAULT=1 "$perl" -MCPAN \
    -e 'CPAN::Shell->notest("install", "YAML::Tiny", "File::HomeDir")' >"$log" 2>&1 || true
  if "$li" --version >/dev/null 2>&1; then
    ok "latexindent works"
  else
    note "latexindent still lacks Perl modules (see $log)"
  fi
}

choose_tex() {
  [ -n "$TEX_CHOICE" ] && return 0
  if ! interactive; then
    TEX_CHOICE=recommended
    return 0
  fi
  printf '\n%sTeX%s (Lemma builds documents with latexmk):\n' "$BOLD" "$OFF" >/dev/tty
  printf '  1) Recommended: BasicTeX, Lemma'"'"'s tools and the common packages (about 0.6 GB)\n' >/dev/tty
  printf '  2) Full: all of MacTeX, with its apps (7 GB download, 9 GB on disk)\n' >/dev/tty
  printf '  3) None: I install TeX myself\n' >/dev/tty
  case "$(ask "Choose [1]:" 1)" in
    2 | full) TEX_CHOICE=full ;;
    3 | none) TEX_CHOICE=none ;;
    *) TEX_CHOICE=recommended ;;
  esac
}

install_tex() {
  case "$TEX_CHOICE" in
    recommended | full | none) ;;
    *) die "--tex takes recommended, full or none (not $TEX_CHOICE)" ;;
  esac
  if [ "$TEX_CHOICE" = none ]; then
    note "no TeX installed: Lemma needs latexmk to build documents"
    return 0
  fi
  step "TeX"
  if [ -x "$TEX_BIN/tlmgr" ]; then
    ok "TeX Live is installed: $("$TEX_BIN/pdflatex" --version 2>/dev/null | head -n 1)"
  elif command -v latexmk >/dev/null && command -v pdflatex >/dev/null; then
    ok "TeX is installed ($(command -v pdflatex)); it is left as it is"
    local t
    for t in biber chktex texcount latexindent; do
      command -v "$t" >/dev/null || note "$t is missing from your TeX installation"
    done
    return 0
  elif [ "$TEX_CHOICE" = full ]; then
    local free_kb
    free_kb="$(df -k / | awk 'NR == 2 { print $4 }')"
    [ "${free_kb:-0}" -gt $((20 * 1024 * 1024)) ] ||
      die "MacTeX needs about 20 GB free (download and installation); choose --tex recommended"
    install_texlive_package MacTeX MacTeX.pkg
  else
    install_texlive_package BasicTeX BasicTeX.pkg
  fi
  export PATH="$TEX_BIN:$PATH"
  step "TeX packages"
  if [ "$TEX_CHOICE" = full ]; then
    tlmgr_add "$TEX_TOOLS"
  else
    tlmgr_add "$TEX_TOOLS $TEX_COMMON"
  fi
  fix_latexindent
  local t
  for t in latexmk biber chktex texcount; do
    [ -x "$TEX_BIN/$t" ] || note "$t is missing: sudo tlmgr install $t"
  done
  "$TEX_BIN/kpsewhich" mylatexformat.ltx >/dev/null 2>&1 ||
    note "mylatexformat is missing (math previews are slower): sudo tlmgr install mylatexformat"
}

# ---------- uninstall ----------

uninstall() {
  step "Removing Lemma"
  local dir
  for dir in ${LEMMA_APP_DIR:+"$LEMMA_APP_DIR"} /Applications "$HOME/Applications"; do
    if [ -d "$dir/Lemma.app" ]; then
      rm -rf "$dir/Lemma.app" || sudo rm -rf "$dir/Lemma.app"
      ok "removed $dir/Lemma.app"
    fi
  done
  if [ -f "$COMMAND_PATH" ] && grep -qF "$COMMAND_MARK" "$COMMAND_PATH"; then
    rm -f "$COMMAND_PATH" 2>/dev/null || sudo rm -f "$COMMAND_PATH"
    ok "removed $COMMAND_PATH"
  fi
  rm -rf "$HOME/Library/Caches/lemma-install"
  info "Your settings stay in ~/.config/lemma and ~/.local/state/lemma."
  info "TeX stays too. To remove it: sudo rm -rf /usr/local/texlive /Library/TeX /etc/paths.d/TeX"
}

# ---------- main ----------

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --tex) TEX_CHOICE="${2:?--tex needs recommended, full or none}"; shift ;;
      --tex=*) TEX_CHOICE="${1#--tex=}" ;;
      --no-tex) TEX_CHOICE=none ;;
      --from-source) FROM_SOURCE=1 ;;
      --dmg) DMG="${2:?--dmg needs a file}"; shift ;;
      --dmg=*) DMG="${1#--dmg=}" ;;
      --release) RELEASE="${2:?--release needs a tag}"; shift ;;
      --release=*) RELEASE="${1#--release=}" ;;
      -y | --yes) YES=1 ;;
      --uninstall) UNINSTALL=1 ;;
      -h | --help) usage; exit 0 ;;
      *) die "unknown option: $1 (see --help)" ;;
    esac
    shift
  done
  if [ -n "$DMG" ]; then
    [ -f "$DMG" ] || die "no such file: $DMG"
    DMG="$(cd "$(dirname "$DMG")" && pwd)/$(basename "$DMG")"
  fi
}

main() {
  parse_args "$@"
  check_mac
  trap cleanup EXIT
  trap 'exit 130' INT TERM
  if [ -n "$UNINSTALL" ]; then
    uninstall
    return 0
  fi
  printf '%sLemma for macOS%s: the app, a lemma command and TeX.\n' "$BOLD" "$OFF"
  choose_tex
  if [ "$TEX_CHOICE" != none ] && [ ! -x "$TEX_BIN/latexmk" ] && ! command -v latexmk >/dev/null &&
    ! is_admin; then
    die "installing TeX needs an administrator account; ask one to run this, or use --tex none"
  fi
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/lemma-install.XXXXXX")"
  install_lemma
  install_command
  install_tex

  step "Done"
  ok "Lemma is in $APP"
  info "Start it from Applications or Launchpad, or in Terminal: lemma path/to/main.tex"
  if [ "$TEX_CHOICE" != none ] && [ -x "$TEX_BIN/latexmk" ]; then
    info "TeX is in $TEX_BIN (new Terminal windows find it). More packages: sudo tlmgr install NAME"
  fi
  if [ -n "$NOTES" ]; then
    printf '\n%sNotes:%s\n%s' "$YELLOW" "$OFF" "$NOTES"
  fi
}

main "$@"
