# Packaging Lemma for macOS

`bundle.sh` builds a self-contained `Lemma.app` and a DMG from Homebrew's GTK stack. CI does the
same on every push to `main` and every pull request (`.github/workflows/macos.yml`), and for each
release (see below). `install.sh` is the one-command installer of the main README: it installs
the latest release's DMG (or builds with `bundle.sh` when there is none), a `lemma` command and
TeX.

```sh
brew install gtk4 libadwaita gtksourceview5 libspelling poppler adwaita-icon-theme librsvg pkgconf
packaging/macos/bundle.sh                 # --skip-build: reuse the build of an earlier run
packaging/macos/smoke-test.sh dist/Lemma.app   # CI only: moves Homebrew aside with sudo
```

Output: `dist/Lemma.app` and `dist/Lemma-<version>-macos-<arch>.dmg` (`LEMMA_DIST_DIR`,
`LEMMA_DMG_NAME` change them).

## What the bundle holds

```
Lemma.app/Contents/
  Info.plist                 from Info.plist.in; opens .tex files (UTI org.tug.tex)
  MacOS/lemma, lemma-mcp     lemma finds lemma-mcp next to itself
  Resources/
    lemma.icns               from packaging/io.github.lemma_editor.Lemma.svg
    lib/*.dylib              every non-system library, rewritten to @executable_path/../Resources/lib/
    lib/gdk-pixbuf-2.0/…     image loaders and loaders.cache.in (paths filled in at start-up)
    lib/enchant-2/*.so       spelling providers (AppleSpell uses the system dictionaries)
    lib/gio/modules/         empty: no GIO module is loaded from elsewhere
    share/glib-2.0/schemas   GTK's GSettings schemas, compiled
    share/icons/             Adwaita and hicolor
    share/gtksourceview-5/   language definitions and style schemes
    share/lemma/             the default snippets, prompts and templates, as examples
```

Lemma sets up the environment for these files itself when it runs from a bundle
(`crates/lemma-app/src/macos.rs`), and completes `PATH` for an app started from Finder.

- **Libraries** live in `Resources/lib`, not `Frameworks`: enchant (built relocatable) finds its
  providers by replacing the trailing `lib` of its own directory. `bundle_dylibs.py` copies the
  dependencies of the executables and modules, rewrites every reference, and fails if anything
  still points outside the bundle.
- **Signing** is ad hoc (`codesign --sign -`). arm64 macOS requires a signature, and
  `install_name_tool` breaks the original ones. Without a Developer ID and notarization,
  Gatekeeper blocks the first start; see the main README.
- **Minimum macOS** is the major version of the Mac that builds it, because Homebrew's bottles
  are built for it (`MACOSX_DEPLOYMENT_TARGET` overrides; `LSMinimumSystemVersion` follows it).
  CI builds on `macos-15`, so the DMG needs macOS 15 or later.
- **SyncTeX** is compiled from `crates/lemma-synctex/vendor/synctex` (no libsynctex on macOS).
  Its notice, which its licence asks to go with the program, is `Resources/LICENSE.synctex`
  (printed from the vendored files by `packaging/synctex-notice.sh`), beside Lemma's `LICENSE`.
- **Other licences:** `Resources/LICENSE.crates` has the licences of the Rust crates compiled
  into the two programs (`cargo run -p xtask -- licenses`). `Resources/licenses/` has the
  licence files of every Homebrew formula a library, a module or data in the app came from, a
  folder per formula, and `homebrew-formulae.txt` with their versions (`bundle_dylibs.py`
  takes them from the top of each formula's keg; a formula without such a file is listed as
  having none, and the build prints a warning).

## Releases

Pushing a release tag runs `.github/workflows/release.yml`, which calls `macos.yml` for the DMG
and, once every package has passed its install test, publishes the release with the DMG under two
names: `Lemma-<tag>-macos-arm64.dmg`, and `Lemma-macos-arm64.dmg`, the name `install.sh`
downloads through `releases/latest/download/`. Every run keeps the DMG and the smoke test's
window snapshots (`dist/smoke/*.png`) as the run's artifact `Lemma-macos-arm64`.

## install.sh

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Dharmavati213/lemma/main/packaging/macos/install.sh)"
```

1. Lemma: the release's DMG (`--release TAG`, `--dmg FILE`), or a build from source in
   `~/Library/Caches/lemma-install/src` (`--from-source`, or when no release has a DMG), which
   installs Xcode's command line tools, Homebrew and Rust when they are missing. The app goes to
   `/Applications` (else `~/Applications`), a `lemma` wrapper script to `/usr/local/bin` (a
   wrapper, not a symlink: Lemma finds `lemma-mcp` next to the path it was started from).
2. TeX (`--tex`): `recommended` downloads BasicTeX from CTAN, checks its Apple developer
   signature, installs it and adds `TEX_TOOLS` and `TEX_COMMON` with `tlmgr` (only the missing
   ones); `full` installs MacTeX; `none` skips it. An installed TeX Live only gets the missing
   packages; another TeX (Homebrew's, MacPorts') is left alone.
3. latexindent: macOS's Perl lacks YAML::Tiny and File::HomeDir, which it installs from CPAN.

Everything runs from `main` at the end of the file, so a partly downloaded script does nothing.
`--uninstall` removes the app and the command. `LEMMA_TEX_BIN`, `LEMMA_COMMAND_PATH` and
`LEMMA_APP_DIR` move the TeX, command and app locations (for testing the script).
