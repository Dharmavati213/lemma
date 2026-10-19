# Linux packages

`package.sh` turns a release build into the Linux files of a release. CI builds each one on its
own distribution and installs it in a fresh container (`.github/workflows/release.yml`).

| Command | File | For | Built on |
|---|---|---|---|
| `package.sh deb` | `lemma_<version>-1_amd64.deb` | Ubuntu 26.04+, Debian sid | Ubuntu 26.04 |
| `package.sh rpm` | `lemma-<version>-1.fc44.x86_64.rpm` | Fedora 44+ | Fedora 44 |
| `package.sh tarball` | `lemma-<label>-linux-x86_64.tar.gz` | other distributions with GNOME 50's libraries | Ubuntu 26.04 |
| `makepkg` in `packaging/` | `lemma-<version>-<rel>-x86_64.pkg.tar.zst` | Arch Linux | Arch |

```sh
LEMMA_SYNCTEX_BUNDLED=1 cargo build --release --locked -p lemma-app -p lemma-mcp
packaging/linux/package.sh deb          # or rpm, tarball; --build runs the cargo build itself
```

- All packages hold the same files as the PKGBUILD: `lemma` and `lemma-mcp` in `bin/` (Lemma
  finds `lemma-mcp` next to itself), the desktop entry and icon, and the default snippets,
  prompts and templates in `share/lemma/` as examples for `~/.config/lemma/`.
- The `.deb`, `.rpm` and tarball are built with the bundled SyncTeX parser
  (`LEMMA_SYNCTEX_BUNDLED=1`), so they need no libsynctex. The Arch package uses the system's.
- Dependencies: `dpkg-shlibdeps` computes the `.deb`'s, `rpmbuild` the `.rpm`'s (from the
  libraries the binaries link). TeX, hunspell dictionaries, `git` and Node.js are Recommends and
  Suggests. The Debian `copyright` file is the GPL text.
- `lemma.spec` packages the staged files only (`rpmbuild --define lemma_stage …`).
- `install.sh` goes into the tarball. It checks the libraries with `ldd` (and by running
  `lemma --version`, which also catches a C library that is too old) and names the packages for
  Arch, Fedora, Debian/Ubuntu and openSUSE when they are missing. It installs into `~/.local`, or
  `/usr/local` with `--system`, writes the desktop entry with the installed binary's path, and
  records what it installed for `--uninstall`.
- `smoke-test.sh LEMMA OUT NAME` starts an installed Lemma on a Broadway display and snapshots
  its window (CI keeps the snapshots as artifacts).
