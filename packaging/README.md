# Packaging Lemma for Arch Linux

`PKGBUILD` builds Lemma from this checkout as it is (no download of the sources) and packages:

- `/usr/bin/lemma` and `/usr/bin/lemma-mcp` (Lemma finds `lemma-mcp` next to itself);
- `/usr/share/applications/io.github.lemma_editor.Lemma.desktop`, which opens `.tex` files
  (`text/x-tex`);
- `/usr/share/icons/hicolor/scalable/apps/io.github.lemma_editor.Lemma.svg`;
- `/usr/share/lemma/`: the built-in default snippets, prompts and document templates, as examples
  for `~/.config/lemma/` (Lemma itself uses the copies compiled into it);
- the licence.

## Build and install

```sh
cd packaging
makepkg -si
```

If Rust comes from rustup's own installer (`~/.cargo/bin`) rather than from pacman, pacman does
not know about `cargo` and `-s` would install pacman's Rust next to it. Install the run
dependencies yourself and skip makepkg's dependency check instead:

```sh
sudo pacman -S --needed gtk4 libadwaita gtksourceview5 libspelling libsynctex poppler-glib
makepkg -i --nodeps
```

`makepkg` runs `cargo build --release --locked` with its target directory in `packaging/src/`, so
the repository's `target/` is left alone. A release build takes several minutes. `-s` installs
the missing build and run dependencies with pacman, `-i` installs the package (both ask for your
password). Without `-i`, `makepkg` only builds `lemma-<version>-x86_64.pkg.tar.zst`, which
`sudo pacman -U lemma-*.pkg.tar.zst` installs later.

Optional dependencies: `texlive-binextra` (latexmk and chktex), `hunspell-en_us` (spell
checking), `nodejs` and `npm` (the Claude Code agent), `git` (the git gutter).

## After installing

- `update-desktop-database` and `gtk-update-icon-cache` run through pacman's hooks, so Lemma shows
  up in the launcher and in "Open With" for `.tex` files.
- To make Lemma the default for `.tex` files:
  `xdg-mime default io.github.lemma_editor.Lemma.desktop text/x-tex`.
- Settings live in `~/.config/lemma/config.toml` (Preferences opens it).

## Updating

Bump `pkgver` (or `pkgrel`) when the workspace version changes, pull, and run `makepkg -si`
again. `makepkg -C` starts from a clean `src/`.
