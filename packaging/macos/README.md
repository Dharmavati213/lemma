# Packaging Lemma for macOS

`bundle.sh` builds a self-contained `Lemma.app` and a DMG from Homebrew's GTK stack. CI does the
same on every push (`.github/workflows/macos.yml`), and attaches the DMG to published releases.

```sh
brew install gtk4 libadwaita gtksourceview5 libspelling poppler adwaita-icon-theme librsvg
packaging/macos/bundle.sh                 # or --skip-build after cargo build --release
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

## Releases

A published GitHub release triggers the workflow, which uploads
`Lemma-<tag>-macos-arm64.dmg` to it. Every other run keeps the DMG and the smoke test's window
snapshots (`dist/smoke/*.png`) as the run's artifact.
